# M1 smoke test: build with
#   aero build --pyext examples/py_bind/py_bind.aero
# then run this script from the directory containing py_bind.pyd.

import py_bind

assert py_bind.add(40, 2) == 42, py_bind.add(40, 2)
assert py_bind.double(3.5) == 7.0, py_bind.double(3.5)
assert py_bind.is_even(4) is True, py_bind.is_even(4)
assert py_bind.is_even(7) is False, py_bind.is_even(7)
assert py_bind.greet("aero") == "aero", py_bind.greet("aero")
assert py_bind.log_message("hi") is None, py_bind.log_message("hi")

print("py_bind smoke: ALL PASS")
