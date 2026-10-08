
use super::*;

fn key(v: &Value, fam: HashFam) -> Option<HashKeyPart> {
    hash_key_part(v, fam)
}

/// v0.93: `=`-equal values hash equal (the hash-consistency
/// invariant the hash join's soundness rests on).
#[test]
fn hash_key_consistency() {
    // int family mixes with numeric: 4 = 4.0 = 4.00.
    let k_int = key(&Value::Int(4), HashFam::ExactNum).unwrap();
    let k_big = key(&Value::BigInt(4), HashFam::ExactNum).unwrap();
    let k_small = key(&Value::SmallInt(4), HashFam::ExactNum).unwrap();
    let k_num = key(
        &Value::Numeric(Numeric::parse("4.00").unwrap()),
        HashFam::ExactNum,
    )
    .unwrap();
    assert_eq!(k_int, k_big);
    assert_eq!(k_int, k_small);
    assert_eq!(k_int, k_num);
    // different values hash different (no requirement, but sanity).
    let k5 = key(&Value::Int(5), HashFam::ExactNum).unwrap();
    assert_ne!(k_int, k5);
    // negative numerics.
    let kn1 = key(
        &Value::Numeric(Numeric::parse("-12.50").unwrap()),
        HashFam::ExactNum,
    )
    .unwrap();
    let kn2 = key(&Value::Int(-25), HashFam::ExactNum).unwrap();
    let kn3 = key(
        &Value::Numeric(Numeric::parse("-12.5").unwrap()),
        HashFam::ExactNum,
    )
    .unwrap();
    assert_eq!(kn1, kn3);
    assert_ne!(kn1, kn2);
    // big numerics (past i128) stay consistent.
    let big1 = Numeric::parse("123456789012345678901234567890.10").unwrap();
    let big2 = Numeric::parse("123456789012345678901234567890.1").unwrap();
    assert_eq!(
        key(&Value::Numeric(big1), HashFam::ExactNum),
        key(&Value::Numeric(big2), HashFam::ExactNum)
    );
    // floats: f32 widened like cmp_ordering.
    let f1 = key(&Value::Float4(1.5), HashFam::Float).unwrap();
    let f2 = key(&Value::Float(1.5), HashFam::Float).unwrap();
    assert_eq!(f1, f2);
    // v0.94 PG19 parity (hashfloat8): -0.0 hashes as 0.0, all
    // NaNs hash as one standard NaN — matching `=` (float8_eq).
    let fnz = key(&Value::Float(-0.0), HashFam::Float).unwrap();
    let fz = key(&Value::Float(0.0), HashFam::Float).unwrap();
    assert_eq!(fnz, fz);
    let fnan1 = key(&Value::Float(f64::NAN), HashFam::Float).unwrap();
    let fnan2 = key(&Value::Float(-f64::NAN), HashFam::Float).unwrap();
    assert_eq!(fnan1, fnan2);
    assert_eq!(
        fnan1,
        key(&Value::Float4(f32::NAN), HashFam::Float).unwrap()
    );
    // v0.94: cross-scale / cross-width exact numerics share one
    // key (PG19 hash_numeric omits trailing zeros; hashint8 is
    // compatible with hashint4/hashint2).
    let k5 = key(&Value::Int(5), HashFam::ExactNum).unwrap();
    assert_eq!(
        k5,
        key(
            &Value::Numeric(Numeric::parse("5.0").unwrap()),
            HashFam::ExactNum
        )
        .unwrap()
    );
    assert_eq!(
        k5,
        key(
            &Value::Numeric(Numeric::parse("5.00").unwrap()),
            HashFam::ExactNum
        )
        .unwrap()
    );
    assert_eq!(k5, key(&Value::BigInt(5), HashFam::ExactNum).unwrap());
    assert_eq!(k5, key(&Value::SmallInt(5), HashFam::ExactNum).unwrap());
    // v0.94: specials — NaN = NaN, distinct from the infinities
    // (PG19 cmp_numerics considers all NaNs equal).
    let knan = key(
        &Value::Numeric(Numeric::parse("NaN").unwrap()),
        HashFam::ExactNum,
    )
    .unwrap();
    assert_eq!(
        knan,
        key(
            &Value::Numeric(Numeric::parse("nan").unwrap()),
            HashFam::ExactNum
        )
        .unwrap()
    );
    assert_ne!(
        knan,
        key(
            &Value::Numeric(Numeric::parse("Infinity").unwrap()),
            HashFam::ExactNum
        )
        .unwrap()
    );
    let kinf = key(
        &Value::Numeric(Numeric::parse("Infinity").unwrap()),
        HashFam::ExactNum,
    )
    .unwrap();
    assert_eq!(
        kinf,
        key(
            &Value::Numeric(Numeric::parse("inf").unwrap()),
            HashFam::ExactNum
        )
        .unwrap()
    );
    // text / bpchar (rtrim) / bool / date / ts / bytea / uuid.
    assert_eq!(
        key(&Value::text("x"), HashFam::Text),
        key(&Value::text("x"), HashFam::Text)
    );
    assert_ne!(
        key(&Value::text("x"), HashFam::Text),
        key(&Value::text("x "), HashFam::Text)
    );
    assert_eq!(
        key(&Value::BpChar("ab  ".into()), HashFam::BpChar),
        key(&Value::BpChar("ab".into()), HashFam::BpChar)
    );
    assert_eq!(
        key(&Value::Bool(true), HashFam::Bool),
        key(&Value::Bool(true), HashFam::Bool)
    );
    assert_eq!(
        key(&Value::Date(42), HashFam::Date),
        key(&Value::Date(42), HashFam::Date)
    );
    assert_eq!(
        key(&Value::Timestamp(7), HashFam::Ts),
        key(&Value::Timestamptz(7), HashFam::Ts)
    );
    assert_eq!(
        key(&Value::Bytea(vec![1, 2]), HashFam::Bytea),
        key(&Value::Bytea(vec![1, 2]), HashFam::Bytea)
    );
    assert_eq!(
        key(&Value::Uuid([9; 16]), HashFam::Uuid),
        key(&Value::Uuid([9; 16]), HashFam::Uuid)
    );
    assert_eq!(
        key(&Value::SingleChar(b'a'), HashFam::Char),
        key(&Value::SingleChar(b'a'), HashFam::Char)
    );
    assert_eq!(
        key(&Value::PgLsn(0x0102), HashFam::PgLsn),
        key(&Value::PgLsn(0x0102), HashFam::PgLsn)
    );
    // wrong-variant values decline (caller falls back to nested loop).
    assert_eq!(key(&Value::text("4"), HashFam::ExactNum), None);
    assert_eq!(key(&Value::Int(4), HashFam::Text), None);
    assert_eq!(key(&Value::Null, HashFam::ExactNum), None);
}

/// v0.93: hash_family maps static types to families; exotic types
/// decline.
#[test]
fn hash_family_mapping() {
    assert_eq!(hash_family(&ColType::Int), Some(HashFam::ExactNum));
    assert_eq!(hash_family(&ColType::BigInt), Some(HashFam::ExactNum));
    assert_eq!(
        hash_family(&ColType::Numeric(None)),
        Some(HashFam::ExactNum)
    );
    assert_eq!(hash_family(&ColType::Float), Some(HashFam::Float));
    assert_eq!(hash_family(&ColType::Text), Some(HashFam::Text));
    assert_eq!(hash_family(&ColType::Varchar(Some(3))), Some(HashFam::Text));
    assert_eq!(hash_family(&ColType::Char(Some(3))), Some(HashFam::BpChar));
    assert_eq!(hash_family(&ColType::Name), None);
    assert_eq!(hash_family(&ColType::Regclass), None);
    assert_eq!(hash_family(&ColType::Json), None);
    assert_eq!(hash_family(&ColType::Record), None);
}

/// v0.93: key extraction from a resolved ON predicate.
#[test]
fn hash_join_key_extraction() {
    use crate::sql::parse_statement;
    // `select * from t1 a join t2 b on a.id = b.id and a.x = b.y`
    let stmt =
        parse_statement("select * from t1 a join t2 b on a.id = b.id and a.x = b.y").unwrap();
    let (from, where_) = match &stmt {
        crate::sql::Stmt::Select(s) => (s.from.clone(), s.where_.clone()),
        _ => panic!("expected select"),
    };
    let lschema = vec![
        QCol {
            qual: "a".into(),
            name: "id".into(),
            ty: ColType::Int,
            hidden: false,
            src_ord: 0,
        },
        QCol {
            qual: "a".into(),
            name: "x".into(),
            ty: ColType::Text,
            hidden: false,
            src_ord: 1,
        },
    ];
    let rschema = vec![
        QCol {
            qual: "b".into(),
            name: "id".into(),
            ty: ColType::BigInt,
            hidden: false,
            src_ord: 0,
        },
        QCol {
            qual: "b".into(),
            name: "y".into(),
            ty: ColType::Text,
            hidden: false,
            src_ord: 1,
        },
    ];
    let on = match &from[0] {
        FromItem::Join { on, .. } => on.clone().unwrap(),
        _ => panic!("expected join"),
    };
    let schemas: Vec<&[QCol]> = vec![&lschema, &rschema];
    let resolved = resolve_predicate_columns(&on, &schemas).unwrap();
    let keys = hash_join_keys(&resolved, 0, &lschema, &rschema).unwrap();
    // split_conjuncts is stack-order (reversed); sort for comparison.
    let mut keys = keys;
    keys.sort();
    assert_eq!(keys, vec![(0, 0, HashFam::ExactNum), (1, 1, HashFam::Text)]);
    // non-equi ON: no keys.
    let stmt2 = parse_statement("select * from t1 a join t2 b on a.id < b.id").unwrap();
    let (from2, _) = match &stmt2 {
        crate::sql::Stmt::Select(s) => (s.from.clone(), s.where_.clone()),
        _ => panic!("expected select"),
    };
    let _ = where_;
    let on2 = match &from2[0] {
        FromItem::Join { on, .. } => on.clone().unwrap(),
        _ => panic!("expected join"),
    };
    let resolved2 = resolve_predicate_columns(&on2, &schemas).unwrap();
    assert_eq!(hash_join_keys(&resolved2, 0, &lschema, &rschema), None);
    // mixed text/int conjunct is not a key; the int/int one is.
    let stmt3 =
        parse_statement("select * from t1 a join t2 b on a.id = b.id and a.x = b.id").unwrap();
    let from3 = match &stmt3 {
        crate::sql::Stmt::Select(s) => s.from.clone(),
        _ => panic!("expected select"),
    };
    let on3 = match &from3[0] {
        FromItem::Join { on, .. } => on.clone().unwrap(),
        _ => panic!("expected join"),
    };
    let resolved3 = resolve_predicate_columns(&on3, &schemas).unwrap();
    let keys3 = hash_join_keys(&resolved3, 0, &lschema, &rschema).unwrap();
    assert_eq!(keys3, vec![(0, 0, HashFam::ExactNum)]);
}
