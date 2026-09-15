//! rippled `PaySteps.cpp`: `toStrand` (normalise a path into implied
//! source / sendMax issuer / deliver / destination elements, then walk the
//! pairs building DirectStepI / BookStep / XRPEndpointStep, each checked as
//! it is made) and `toStrands` (the default path plus every explicit path,
//! de-duplicated, `temRIPPLE_EMPTY` when nothing can be built).
use super::amm::SharedAmmContext;
use super::book_step::BookStep;
use super::direct_step::DirectStep;
use super::payment_sandbox::PaymentSandbox;
use super::quality_function::Quality;
use super::steps::{strands_equal, Asset, OfferCrossing, Step, Strand, StrandContext};
use super::view::StepCheckError;
use super::xrp_endpoint_step::XrpEndpointStep;
use crate::ledger::transactor::TxResult;
use crate::tx::offer::decode20;
use xrpl_core::types::Hash256;

/// `STPathElement` type bits.
pub const TYPE_ACCOUNT: u8 = 0x01;
pub const TYPE_CURRENCY: u8 = 0x10;
pub const TYPE_ISSUER: u8 = 0x20;
const TYPE_ALL: u8 = TYPE_ACCOUNT | TYPE_CURRENCY | TYPE_ISSUER;

/// `STPathElement`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PathElement {
    pub node_type: u8,
    pub account: [u8; 20],
    pub currency: [u8; 20],
    pub issuer: [u8; 20],
}

impl PathElement {
    pub fn account(id: [u8; 20]) -> PathElement {
        PathElement { node_type: TYPE_ACCOUNT, account: id, currency: [0; 20], issuer: [0; 20] }
    }
    pub fn all(account: [u8; 20], currency: [u8; 20], issuer: [u8; 20]) -> PathElement {
        PathElement { node_type: TYPE_ALL, account, currency, issuer }
    }
    pub fn offer(currency: Option<[u8; 20]>, issuer: Option<[u8; 20]>) -> PathElement {
        let mut t = 0u8;
        if currency.is_some() {
            t |= TYPE_CURRENCY;
        }
        if issuer.is_some() {
            t |= TYPE_ISSUER;
        }
        PathElement { node_type: t, account: [0; 20], currency: currency.unwrap_or([0; 20]), issuer: issuer.unwrap_or([0; 20]) }
    }
    pub fn is_account(&self) -> bool {
        self.node_type & TYPE_ACCOUNT != 0
    }
    pub fn is_offer(&self) -> bool {
        !self.is_account()
    }
    pub fn has_currency(&self) -> bool {
        self.node_type & TYPE_CURRENCY != 0
    }
    pub fn has_issuer(&self) -> bool {
        self.node_type & TYPE_ISSUER != 0
    }

    /// One element of a transaction's `Paths` JSON.
    pub fn from_json(v: &serde_json::Value) -> Option<PathElement> {
        let mut t = v.get("type").and_then(|x| x.as_u64()).unwrap_or(0) as u8;
        let account = v.get("account").and_then(|x| x.as_str()).and_then(decode20);
        let currency = v.get("currency").and_then(|x| x.as_str()).and_then(|s| {
            if s == "XRP" {
                Some([0u8; 20])
            } else {
                hex::decode(s).ok().and_then(|b| <[u8; 20]>::try_from(b.as_slice()).ok())
            }
        });
        let issuer = v.get("issuer").and_then(|x| x.as_str()).and_then(decode20);
        if t == 0 {
            if account.is_some() {
                t |= TYPE_ACCOUNT;
            }
            if currency.is_some() {
                t |= TYPE_CURRENCY;
            }
            if issuer.is_some() {
                t |= TYPE_ISSUER;
            }
        }
        Some(PathElement { node_type: t, account: account.unwrap_or([0; 20]), currency: currency.unwrap_or([0; 20]), issuer: issuer.unwrap_or([0; 20]) })
    }
}

fn is_xrp_id(id: &[u8; 20]) -> bool {
    id == &[0u8; 20]
}

/// `noAccount()`: the account id of all ones.
fn is_no_account(id: &[u8; 20]) -> bool {
    id == &[0xFFu8; 20]
}

/// `isConsistent(Issue)`: XRP has no issuer, an IOU has one.
fn is_consistent(asset: &Asset) -> bool {
    asset.is_xrp() == asset.issuer.is_none()
}

/// The `Issue` the walk carries (`curIssue`): currency + account.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Issue {
    currency: [u8; 20],
    account: [u8; 20],
}

impl Issue {
    fn asset(&self) -> Asset {
        if is_xrp_id(&self.currency) { Asset::XRP } else { Asset { currency: self.currency, issuer: Some(self.account) } }
    }
}

fn ter(e: StepCheckError) -> TxResult {
    match e {
        StepCheckError::BadPath | StepCheckError::BadPathLoop => TxResult::BadPath,
        StepCheckError::NoLine => TxResult::NoLine,
        StepCheckError::NoRipple => TxResult::NoRipple,
        StepCheckError::NoAccount => TxResult::NoAccount,
        StepCheckError::NoAuth => TxResult::NoAuth,
        StepCheckError::PathDry => TxResult::PathDry,
        StepCheckError::NoIssuer => TxResult::NoIssuer,
    }
}

/// The inputs `toStrand` needs beyond the path itself.
pub struct StrandInputs<'a> {
    pub src: [u8; 20],
    pub dst: [u8; 20],
    pub deliver: Asset,
    pub limit_quality: Option<Quality>,
    pub send_max_issue: Option<Asset>,
    pub owner_pays_transfer_fee: bool,
    pub offer_crossing: OfferCrossing,
    pub amm_ctx: &'a SharedAmmContext,
    pub domain: Option<Hash256>,
}

struct Builder<'a, 'b, 'c> {
    sb: &'a PaymentSandbox<'b, 'c>,
    inputs: &'a StrandInputs<'a>,
    strand_src: [u8; 20],
    strand_dst: [u8; 20],
    is_default_path: bool,
    result: Strand,
    seen_direct_issues: [Vec<(Asset,)>; 2],
    seen_book_outs: Vec<Asset>,
}

impl<'a, 'b, 'c> Builder<'a, 'b, 'c> {
    fn ctx(&self, is_last: bool) -> StrandContext {
        StrandContext {
            strand_src: self.strand_src,
            strand_dst: self.strand_dst,
            strand_deliver: self.inputs.deliver,
            limit_quality: self.inputs.limit_quality,
            is_first: self.result.is_empty(),
            is_last,
            owner_pays_transfer_fee: self.inputs.owner_pays_transfer_fee,
            offer_crossing: self.inputs.offer_crossing,
            is_default_path: self.is_default_path,
            strand_size: self.result.len(),
            domain: self.inputs.domain,
        }
    }

    fn prev(&self) -> Option<&dyn Step> {
        self.result.last().map(|b| b.as_ref())
    }

    /// `make_DirectStepI` plus the loop checks `DirectStepI::check` keeps
    /// in the context (`seenBookOuts`, `seenDirectIssues`).
    fn make_direct(&mut self, is_last: bool, src: [u8; 20], dst: [u8; 20], currency: [u8; 20]) -> Result<(), TxResult> {
        let ctx = self.ctx(is_last);
        let src_issue = Asset { currency, issuer: Some(src) };
        let dst_issue = Asset { currency, issuer: Some(dst) };
        if self.seen_book_outs.contains(&src_issue) {
            let Some(prev) = self.prev() else { return Err(TxResult::BadPath) };
            match prev.book_step_book() {
                Some(book) if book.output != src_issue => return Err(TxResult::BadPath),
                _ => {}
            }
        }
        if self.seen_direct_issues[0].iter().any(|(a,)| *a == src_issue) || self.seen_direct_issues[1].iter().any(|(a,)| *a == dst_issue) {
            return Err(TxResult::BadPath);
        }
        self.seen_direct_issues[0].push((src_issue,));
        self.seen_direct_issues[1].push((dst_issue,));
        let step = DirectStep::make(&ctx, self.sb, self.prev(), src, dst, currency).map_err(ter)?;
        self.result.push(Box::new(step));
        Ok(())
    }

    fn make_xrp_endpoint(&mut self, is_last: bool, acc: [u8; 20]) -> Result<(), TxResult> {
        let ctx = self.ctx(is_last);
        // fix1781: the XRP issue is seen on the side the endpoint sits.
        let idx = if is_last { 0 } else { 1 };
        if self.seen_direct_issues[idx].iter().any(|(a,)| *a == Asset::XRP) {
            return Err(TxResult::BadPath);
        }
        self.seen_direct_issues[idx].push((Asset::XRP,));
        let step = XrpEndpointStep::make(&ctx, self.sb, acc).map_err(ter)?;
        self.result.push(Box::new(step));
        Ok(())
    }

    /// `make_BookStep*` with `BookStep::check`: same-issue, the loop
    /// tests on `seenBookOuts` / `seenDirectIssues`, issuer existence, and
    /// the previous DirectStep's line and NoRipple test.
    fn make_book(&mut self, is_last: bool, input: Asset, output: Asset) -> Result<(), TxResult> {
        let ctx = self.ctx(is_last);
        if input == output || !is_consistent(&input) || !is_consistent(&output) {
            return Err(TxResult::BadPath);
        }
        if self.seen_book_outs.contains(&output) || self.seen_direct_issues[0].iter().any(|(a,)| *a == output) {
            return Err(TxResult::BadPath);
        }
        self.seen_book_outs.push(output);
        if self.seen_direct_issues[1].iter().any(|(a,)| *a == output) {
            return Err(TxResult::BadPath);
        }
        let issuer_exists = |a: &Asset| match a.issuer {
            None => true,
            Some(id) => crate::tx::offer::json_at(self.sb.sandbox(), &crate::ledger::keylet::account_root_key(&id)).is_some(),
        };
        if !issuer_exists(&input) || !issuer_exists(&output) {
            return Err(TxResult::NoIssuer);
        }
        if let Some(prev) = self.prev() {
            if let Some(prev_src) = prev.direct_step_src_acct() {
                let cur = input.issuer.unwrap_or([0; 20]);
                let Some(line) = crate::tx::offer::json_at(self.sb.sandbox(), &crate::ledger::keylet::ripple_state_key(&prev_src, &cur, &input.currency)) else {
                    return Err(TxResult::NoLine);
                };
                let bit: u64 = if cur > prev_src { 0x0020_0000 } else { 0x0010_0000 };
                if line["Flags"].as_u64().unwrap_or(0) & bit != 0 {
                    return Err(TxResult::NoRipple);
                }
            }
        }
        let step = BookStep::new(&ctx, input, output, self.prev(), self.sb, self.inputs.amm_ctx);
        if !step.check() {
            return Err(TxResult::BadPath);
        }
        self.result.push(Box::new(step));
        Ok(())
    }

    /// `toStep(ctx, e1, e2, curIssue)`.
    fn to_step(&mut self, is_last: bool, e1: &PathElement, e2: &PathElement, cur: Issue) -> Result<(), TxResult> {
        let is_first = self.result.is_empty();
        if is_first && e1.is_account() && e1.has_currency() && is_xrp_id(&e1.currency) {
            return self.make_xrp_endpoint(is_last, e1.account);
        }
        if is_last && e1.is_account() && is_xrp_id(&e1.account) && e2.is_account() {
            return self.make_xrp_endpoint(is_last, e2.account);
        }
        if e1.is_account() && e2.is_account() {
            return self.make_direct(is_last, e1.account, e2.account, cur.currency);
        }
        if e1.is_offer() && e2.is_account() {
            return Err(TxResult::BadPath);
        }
        let out_currency = if e2.has_currency() { e2.currency } else { cur.currency };
        let out_issuer = if e2.has_issuer() { e2.issuer } else { cur.account };
        if is_xrp_id(&cur.currency) && is_xrp_id(&out_currency) {
            return Err(TxResult::BadPath);
        }
        let output = if is_xrp_id(&out_currency) { Asset::XRP } else { Asset { currency: out_currency, issuer: Some(out_issuer) } };
        self.make_book(is_last, cur.asset(), output)
    }
}

/// `toStrand(view, src, dst, deliver, limitQuality, sendMaxIssue, path,
/// ownerPaysTransferFee, offerCrossing, ammContext, domainID)`.
pub fn to_strand(sb: &PaymentSandbox, inputs: &StrandInputs, path: &[PathElement]) -> Result<Strand, TxResult> {
    let (src, dst, deliver) = (inputs.src, inputs.dst, inputs.deliver);
    if is_xrp_id(&src) || is_xrp_id(&dst) || !is_consistent(&deliver) || inputs.send_max_issue.is_some_and(|s| !is_consistent(&s)) {
        return Err(TxResult::BadPath);
    }
    if inputs.send_max_issue.and_then(|s| s.issuer).is_some_and(|a| is_no_account(&a)) || is_no_account(&src) || is_no_account(&dst) || deliver.issuer.is_some_and(|a| is_no_account(&a)) {
        return Err(TxResult::BadPath);
    }
    for pe in path {
        let t = pe.node_type;
        if (t & !TYPE_ALL) != 0 || t == 0 {
            return Err(TxResult::BadPath);
        }
        let (has_account, has_issuer, has_currency) = (pe.is_account(), pe.has_issuer(), pe.has_currency());
        if has_account && (has_issuer || has_currency) {
            return Err(TxResult::BadPath);
        }
        if has_issuer && is_xrp_id(&pe.issuer) {
            return Err(TxResult::BadPath);
        }
        if has_account && is_xrp_id(&pe.account) {
            return Err(TxResult::BadPath);
        }
        if has_currency && has_issuer && is_xrp_id(&pe.currency) != is_xrp_id(&pe.issuer) {
            return Err(TxResult::BadPath);
        }
        if has_issuer && is_no_account(&pe.issuer) {
            return Err(TxResult::BadPath);
        }
        if has_account && is_no_account(&pe.account) {
            return Err(TxResult::BadPath);
        }
    }
    let mut cur = {
        let currency = inputs.send_max_issue.map(|s| s.currency).unwrap_or(deliver.currency);
        if is_xrp_id(&currency) { Issue { currency: [0; 20], account: [0; 20] } } else { Issue { currency, account: src } }
    };
    let deliver_account = deliver.issuer.unwrap_or([0; 20]);
    // Normalise the path.
    let mut norm: Vec<PathElement> = Vec::with_capacity(4 + path.len());
    norm.push(PathElement::all(src, cur.currency, cur.account));
    if let Some(smi) = inputs.send_max_issue {
        let sm_account = smi.issuer.unwrap_or([0; 20]);
        if sm_account != src && (path.is_empty() || !path[0].is_account() || path[0].account != sm_account) {
            norm.push(PathElement::account(sm_account));
        }
    }
    norm.extend_from_slice(path);
    {
        let last_currency = norm.iter().rev().find(|pe| pe.has_currency()).copied().unwrap_or(norm[0]);
        if last_currency.currency != deliver.currency || (inputs.offer_crossing != OfferCrossing::No && last_currency.issuer != deliver_account) {
            norm.push(PathElement::offer(Some(deliver.currency), Some(deliver_account)));
        }
    }
    {
        let back = norm[norm.len() - 1];
        if !((back.is_account() && back.account == deliver_account) || dst == deliver_account) {
            norm.push(PathElement::account(deliver_account));
        }
    }
    {
        let back = norm[norm.len() - 1];
        if !back.is_account() || back.account != dst {
            norm.push(PathElement::account(dst));
        }
    }
    if norm.len() < 2 {
        return Err(TxResult::BadPath);
    }
    let strand_src = norm[0].account;
    let strand_dst = norm[norm.len() - 1].account;
    let mut b = Builder {
        sb,
        inputs,
        strand_src,
        strand_dst,
        is_default_path: path.is_empty(),
        result: Vec::with_capacity(2 * norm.len()),
        seen_direct_issues: [Vec::new(), Vec::new()],
        seen_book_outs: Vec::new(),
    };
    let n = norm.len();
    let mut i = 0usize;
    while i + 1 < n {
        let mut cur_pe = norm[i];
        let next = norm[i + 1];
        if cur_pe.is_account() {
            cur.account = cur_pe.account;
        } else if cur_pe.has_issuer() {
            cur.account = cur_pe.issuer;
        }
        if cur_pe.has_currency() {
            cur.currency = cur_pe.currency;
            if is_xrp_id(&cur.currency) {
                cur.account = [0; 20];
            }
        }
        if cur_pe.is_account() && next.is_account() {
            if !is_xrp_id(&cur.currency) && cur.account != cur_pe.account && cur.account != next.account {
                // Inserting implied account.
                b.make_direct(false, cur_pe.account, cur.account, cur.currency)?;
                cur_pe = PathElement::account(cur.account);
            }
        } else if cur_pe.is_account() && next.is_offer() {
            if cur.account != cur_pe.account {
                // Inserting implied account before offer.
                b.make_direct(false, cur_pe.account, cur.account, cur.currency)?;
                cur_pe = PathElement::account(cur.account);
            }
        } else if cur_pe.is_offer() && next.is_account() {
            if cur.account != next.account && !is_xrp_id(&next.account) {
                if is_xrp_id(&cur.currency) {
                    if i != n - 2 {
                        return Err(TxResult::BadPath);
                    }
                    // Last step: insert the XRP endpoint step.
                    b.make_xrp_endpoint(true, next.account)?;
                } else {
                    // Inserting implied account after offer.
                    b.make_direct(i == n - 2, cur.account, next.account, cur.currency)?;
                }
            }
            i += 1;
            continue;
        }
        if !next.is_offer() && next.has_currency() && next.currency != cur.currency {
            return Err(TxResult::BadPath);
        }
        b.to_step(i == n - 2, &cur_pe, &next, cur)?;
        i += 1;
    }
    // `checkStrand`: accounts chain from src to dst, issues chain through.
    {
        let mut cur_acc = src;
        let mut cur_iss = {
            let currency = inputs.send_max_issue.map(|s| s.currency).unwrap_or(deliver.currency);
            if is_xrp_id(&currency) { Issue { currency: [0; 20], account: [0; 20] } } else { Issue { currency, account: src } }
        };
        for s in b.result.iter() {
            let accts = match s.direct_step_accts() {
                Some(a) => a,
                None => match s.book_step_book() {
                    Some(bk) => (bk.input.issuer.unwrap_or([0; 20]), bk.output.issuer.unwrap_or([0; 20])),
                    None => return Err(TxResult::BadPath),
                },
            };
            if accts.0 != cur_acc {
                return Err(TxResult::BadPath);
            }
            if let Some(bk) = s.book_step_book() {
                if cur_iss.asset() != bk.input {
                    return Err(TxResult::BadPath);
                }
                cur_iss = Issue { currency: bk.output.currency, account: bk.output.issuer.unwrap_or([0; 20]) };
            } else {
                cur_iss.account = accts.1;
            }
            cur_acc = accts.1;
        }
        if cur_acc != dst || cur_iss.currency != deliver.currency || (cur_iss.account != deliver_account && cur_iss.account != dst) {
            return Err(TxResult::BadPath);
        }
    }
    Ok(b.result)
}

/// `toStrands(...)`: the default path (when allowed) and each explicit
/// path; `temRIPPLE_EMPTY` with neither; a malformed (tem) path is fatal,
/// a failed one is skipped until none is left.
pub fn to_strands(sb: &PaymentSandbox, inputs: &StrandInputs, paths: &[Vec<PathElement>], add_default_path: bool) -> Result<Vec<Strand>, TxResult> {
    let mut result: Vec<Strand> = Vec::with_capacity(1 + paths.len());
    let mut insert = |s: Strand, result: &mut Vec<Strand>| {
        if !result.iter().any(|r| strands_equal(r, &s)) {
            result.push(s);
        }
    };
    if add_default_path {
        match to_strand(sb, inputs, &[]) {
            Ok(strand) => {
                if strand.is_empty() {
                    return Err(TxResult::FailedProcessing);
                }
                insert(strand, &mut result);
            }
            Err(t) => {
                if is_tem_malformed(t) || paths.is_empty() {
                    return Err(t);
                }
            }
        }
    } else if paths.is_empty() {
        return Err(TxResult::RippleEmpty);
    }
    let mut last_fail = TxResult::Success;
    for p in paths {
        match to_strand(sb, inputs, p) {
            Ok(strand) => {
                if strand.is_empty() {
                    return Err(TxResult::FailedProcessing);
                }
                insert(strand, &mut result);
            }
            Err(t) => {
                last_fail = t;
                if is_tem_malformed(t) {
                    return Err(t);
                }
            }
        }
    }
    if result.is_empty() {
        return Err(last_fail);
    }
    Ok(result)
}

fn is_tem_malformed(t: TxResult) -> bool {
    matches!(t, TxResult::BadPath | TxResult::RippleEmpty)
}
