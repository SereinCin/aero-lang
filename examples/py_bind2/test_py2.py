# M2 smoke test: build with
#   aero build --pyext examples/py_bind2/py_bind2.aero
# then run this script from the directory containing py_bind2.pyd.

import py_bind2

# bytes -> Aero String -> i64
assert py_bind2.bytes_len(b"hello") == 5, py_bind2.bytes_len(b"hello")
assert py_bind2.bytes_len(b"") == 0, py_bind2.bytes_len(b"")

# bytes -> byte value (0-255)
assert py_bind2.bytes_first(b"abc") == 97, py_bind2.bytes_first(b"abc")
assert py_bind2.bytes_first(b"") == -1, py_bind2.bytes_first(b"")

# bytes -> reversed bytes (String return via PyBytes)
assert py_bind2.bytes_reverse(b"abc") == b"cba", py_bind2.bytes_reverse(b"abc")
assert py_bind2.bytes_reverse(b"\x00\x01\x02") == b"\x02\x01\x00", py_bind2.bytes_reverse(b"\x00\x01\x02")

# echo round-trip
assert py_bind2.bytes_echo(b"payload") == b"payload", py_bind2.bytes_echo(b"payload")

print("py_bind2 smoke: ALL PASS")
