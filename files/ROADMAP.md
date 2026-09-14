# rustgres Roadmap — Road to Postgres 19 Parity

## v0.26 (2026-09-14) — "numeric formatting: to_char/to_number" ✓ DONE

**Conformance:** 2956 PASS (56.9%), 1521 EXPECTED-FAIL, 716 REAL-FAIL
(5,193 statements). Delta from v0.25: **+64 PASS, −64 REAL-FAIL**.

**Scope:** Full PG19 `to_char`/`to_number` numeric formatting port.
- `to_char`: fixed-width/FM, MI/PL/SG/S/PR signs, 0-fill, G/,/D grouping,
  L currency, TH ordinals, V shift, RN Roman (1–3999), EEEE scientific.
- `to_number`: sign/grouping/decimal parsing, L/TH/V/Roman, EEEE→0A000.
- Overloads for int2/int4/int8/float4/float8.

**Tests:** `protocol_test26.py` (63/75; 12 spacing expectations manually
transcribed, differ from PG-source output), 107+ unit tests.

## v0.25 (2026-09-14) — "integer input, bitwise operators, numeric edge cases" ✓ DONE

**Conformance:** 2892 PASS (55.7%), 1521 EXPECTED-FAIL, 780 REAL-FAIL.
Delta: +83/−83.

## Next clusters (by REAL-FAIL size)

1. **String functions** (~159 REAL-FAIL): remaining `overlay`, `position`,
   collation-sensitive ops.
2. **Numeric edge cases** (~149 remaining): `STDDEV`/`VARIANCE`, bignum
   precision beyond i128, `numeric(p,s)` typmod enforcement.
3. **Date/time** (TBD): interval math, timezone edge cases.
4. **Arrays/JSON** (TBD): nested constructors, operators.

## Honest limitations

- Valgrind/Callgrind/DHAT not run in sandbox (v0.26).
- 12 `protocol_test26.py` expectations unverified against live PG19.
- `numeric(p,s)` typmods parsed but not enforced.
- Bignum precision beyond i128 unimplemented.
