//! A program that reads its group id must see the host's group base, so that a
//! dispatch the host runs in pieces moves its ids (`VICTORY_GROUP_SPLIT=0` keeps
//! the console's).

use super::*;


/// A program that reads its group id reads the host's base too, so that a
/// dispatch the host runs in pieces moves its ids; `VICTORY_GROUP_SPLIT=0`
/// leaves the id as the console's.
#[test]
fn a_program_that_reads_its_group_id_gets_the_group_base() {
    let words = [
        0xBF06_8000, // s_cmp_eq_u32 s0, 0 (s0 = group id X)
        0xBF85_0001, // s_cbranch_scc1 -> s_endpgm
        0xBE83_0380, // s_mov_b32 s3, 0
        S_ENDPGM,
    ];
    let group_id_x = 1 << 7;
    let base = |l: &Lowered| {
        l.user_data
            .iter()
            .any(|u| u.root == RootType::Other(USER_GROUP_BASE))
    };
    TEST_GROUP_SPLIT.set(Some(true));
    assert!(base(&lowers_cs(&words, group_id_x, false, [1, 1, 1]).expect("group id")));
    TEST_GROUP_SPLIT.set(Some(false));
    assert!(!base(&lowers_cs(&words, group_id_x, false, [1, 1, 1]).expect("group id")));
    TEST_GROUP_SPLIT.set(None);
}
