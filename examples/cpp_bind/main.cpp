// M6 C++ bindgen demo: `aero build --cpp examples/cpp_bind/cpp_bind.aero`
// generates cpp_bind.dll + cpp_bind.hpp; compile this file with
//   g++ main.cpp -I. cpp_bind.dll -o cpp_main
// (MinGW links the DLL directly; on other platforms use -L. -lcpp_bind).

#include <cassert>
#include <cstdio>

#include "cpp_bind.hpp"

int main() {
    assert(add(40, 2) == 42);
    assert(double_(3.5) == 7.0);          // C++ keyword `double` -> `double_`
    assert(is_even(10) == true);
    assert(is_even(7) == false);
    assert(str_len("hello") == 5);        // str -> const char*
    std::printf("cpp_bind smoke: ALL PASS\n");
    return 0;
}
