use std::cmp::Ordering;

fn main() {
    let mut normal_bytes = vec![0u8; 16];
    normal_bytes[7] = 1; // shortroomid = 1
    normal_bytes[15] = 1; // normal(1)

    let mut backfilled_bytes = vec![0u8; 24];
    backfilled_bytes[7] = 1; // shortroomid = 1
    backfilled_bytes[8..16].copy_from_slice(&0u64.to_be_bytes()); // zero tag
    backfilled_bytes[16..24].copy_from_slice(&(-1i64).to_be_bytes()); // backfilled(-1)

    println!("Normal(1) vs Backfilled(-1): {:?}", normal_bytes.cmp(&backfilled_bytes));

    let mut normal_zero = vec![0u8; 16];
    normal_zero[7] = 1;
    normal_zero[15] = 0;

    println!("Normal(0) vs Backfilled(-1): {:?}", normal_zero.cmp(&backfilled_bytes));
}
