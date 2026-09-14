// Track 1 — arithmetic oracle. One C export per STAmount / Number operation so a
// Rust property test can throw random operands at libxrpl and at our own port
// and compare the last digit. Pure functions of their arguments: no ledger,
// no view, no network. Errors (rippled throws on overflow / division by zero)
// come back as a non-zero return with the message in xrpl_arith_last_error().
#include "xrpl_shim.h"

#include <xrpl/basics/Number.h>
#include <xrpl/protocol/Issue.h>
#include <xrpl/protocol/STAmount.h>

#include <cstring>
#include <exception>
#include <string>

namespace {
thread_local std::string g_last_error;

xrpl::STAmount from_amt(XrplAmt const& a) {
    if (a.native) {
        return xrpl::STAmount(static_cast<std::uint64_t>(a.mantissa), a.negative != 0);
    }
    return xrpl::STAmount(xrpl::noIssue(), static_cast<std::uint64_t>(a.mantissa), a.exponent, a.negative != 0);
}

void to_amt(xrpl::STAmount const& r, XrplAmt* out) {
    out->mantissa = r.mantissa();
    out->exponent = r.exponent();
    out->negative = r.negative() ? 1 : 0;
    out->native = r.native() ? 1 : 0;
}

xrpl::Asset result_asset(uint8_t native) {
    return native ? xrpl::Asset{xrpl::xrpIssue()} : xrpl::Asset{xrpl::noIssue()};
}

xrpl::Number::RoundingMode mode_of(int m) {
    switch (m) {
        case 1: return xrpl::Number::RoundingMode::TowardsZero;
        case 2: return xrpl::Number::RoundingMode::Downward;
        case 3: return xrpl::Number::RoundingMode::Upward;
        default: return xrpl::Number::RoundingMode::ToNearest;
    }
}
}  // namespace

extern "C" {

const char* xrpl_arith_last_error(void) {
    return g_last_error.c_str();
}

int xrpl_arith_stamount_op(int op, const XrplAmt* a, const XrplAmt* b, uint8_t round_up, uint8_t result_native, XrplAmt* out) {
    try {
        xrpl::STAmount const x = from_amt(*a);
        xrpl::STAmount const y = from_amt(*b);
        xrpl::Asset const asset = result_asset(result_native);
        xrpl::STAmount r;
        switch (op) {
            case XRPL_ARITH_MULROUND:        r = xrpl::mulRound(x, y, asset, round_up != 0); break;
            case XRPL_ARITH_MULROUND_STRICT: r = xrpl::mulRoundStrict(x, y, asset, round_up != 0); break;
            case XRPL_ARITH_DIVROUND:        r = xrpl::divRound(x, y, asset, round_up != 0); break;
            case XRPL_ARITH_DIVROUND_STRICT: r = xrpl::divRoundStrict(x, y, asset, round_up != 0); break;
            case XRPL_ARITH_MULTIPLY:        r = xrpl::multiply(x, y, asset); break;
            case XRPL_ARITH_DIVIDE:          r = xrpl::divide(x, y, asset); break;
            case XRPL_ARITH_ADD:             r = x + y; break;
            case XRPL_ARITH_SUB:             r = x - y; break;
            case XRPL_ARITH_CANONICALIZE:    r = x; break;
            default: g_last_error = "unknown op"; return 2;
        }
        to_amt(r, out);
        return 0;
    } catch (std::exception const& e) {
        g_last_error = e.what();
        return 1;
    } catch (...) {
        g_last_error = "unknown exception";
        return 1;
    }
}

uint64_t xrpl_arith_get_rate(const XrplAmt* offer_out, const XrplAmt* offer_in) {
    try {
        return xrpl::getRate(from_amt(*offer_out), from_amt(*offer_in));
    } catch (std::exception const& e) {
        g_last_error = e.what();
        return 0;
    }
}

int xrpl_arith_number_op(int op, int64_t m1, int32_t e1, int64_t m2, int32_t e2, int rounding_mode, int64_t* out_m, int32_t* out_e) {
    auto const saved = xrpl::Number::setround(mode_of(rounding_mode));
    try {
        xrpl::Number const a(m1, e1);
        xrpl::Number const b(m2, e2);
        xrpl::Number r;
        switch (op) {
            case XRPL_NUM_ADD: r = a + b; break;
            case XRPL_NUM_SUB: r = a - b; break;
            case XRPL_NUM_MUL: r = a * b; break;
            case XRPL_NUM_DIV: r = a / b; break;
            case XRPL_NUM_ROOT2: r = xrpl::root(a, 2); break;
            default: xrpl::Number::setround(saved); g_last_error = "unknown op"; return 2;
        }
        *out_m = r.mantissa();
        *out_e = r.exponent();
        xrpl::Number::setround(saved);
        return 0;
    } catch (std::exception const& e) {
        xrpl::Number::setround(saved);
        g_last_error = e.what();
        return 1;
    }
}

}  // extern "C"
