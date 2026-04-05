# xrpl_shim build — links against vendored libxrpl.a

CXX       := g++
CXXFLAGS  := -std=c++20 -O2 -fPIC -Wall
INCLUDES  := -I vendor/rippled/include -I vendor/rippled/.build/proto -I vendor/rippled/.build/libxrpl
LIBXRPL   := vendor/rippled/.build/libxrpl.a vendor/rippled/.build/libxrpl.libpb.a
LIBS      := -lpthread -ldl

all: test_shim libxrpl_shim.a

xrpl_shim.o: xrpl_shim.cpp xrpl_shim.h
	$(CXX) $(CXXFLAGS) $(INCLUDES) -c xrpl_shim.cpp -o xrpl_shim.o

libxrpl_shim.a: xrpl_shim.o
	ar rcs libxrpl_shim.a xrpl_shim.o

test_shim.o: test_shim.c xrpl_shim.h
	gcc -O2 -c test_shim.c -o test_shim.o

test_shim: xrpl_shim.o test_shim.o $(LIBXRPL)
	$(CXX) $(CXXFLAGS) xrpl_shim.o test_shim.o $(LIBXRPL) $(LIBS) -o test_shim

test: test_shim
	./test_shim

clean:
	rm -f *.o *.a test_shim
