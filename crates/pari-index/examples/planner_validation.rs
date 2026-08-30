#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]

use std::time::Instant;

use pari_core::MinHash32;
use pari_index::{plan_lsh, LshIndex32, LshParams, LshPlanOptions, LSH_PLANNER_MODEL};

const TRIALS: usize = 10_000;
const NUM_PERM: usize = 128;
const SEED: u64 = 7;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let plan = plan_lsh(LshPlanOptions::new(TRIALS as u64, 0.8, NUM_PERM))?;
    println!(
        "{{\"schema_version\":1,\"model\":\"{LSH_PLANNER_MODEL}\",\"trials_per_point\":{TRIALS},\"num_perm\":{NUM_PERM},\"bands\":{},\"rows\":{},\"points\":[",
        plan.params.bands, plan.params.rows
    );

    for (ordinal, similarity) in [0.5, 0.8, 0.9].into_iter().enumerate() {
        let started = Instant::now();
        let observed = observed_candidate_rate(plan.params, similarity, TRIALS, ordinal as u64)?;
        let elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
        let expected = plan
            .candidate_probability(similarity)
            .expect("fixed similarities are valid");
        let separator = if ordinal == 0 { "" } else { "," };
        println!(
            "{separator}{{\"similarity\":{similarity},\"expected_candidate_rate\":{expected},\"observed_candidate_rate\":{observed},\"absolute_error\":{},\"elapsed_ms\":{elapsed_ms}}}",
            (observed - expected).abs()
        );
    }
    println!("]}}");
    Ok(())
}

fn observed_candidate_rate(
    params: LshParams,
    similarity: f64,
    trials: usize,
    salt: u64,
) -> Result<f64, Box<dyn std::error::Error>> {
    let mut state = 0xA076_1D64_78BD_642F_u64 ^ salt;
    let mut references = Vec::with_capacity(trials);
    let mut queries = Vec::with_capacity(trials);
    for _ in 0..trials {
        let mut reference = Vec::with_capacity(NUM_PERM);
        let mut query = Vec::with_capacity(NUM_PERM);
        for _ in 0..NUM_PERM {
            let value = next_u64(&mut state) as u32;
            reference.push(value);
            let sample = next_u64(&mut state) as f64 / u64::MAX as f64;
            if sample < similarity {
                query.push(value);
            } else {
                query.push(value ^ ((next_u64(&mut state) as u32) | 1));
            }
        }
        references.push(MinHash32::from_signature(reference, SEED)?);
        queries.push(MinHash32::from_signature(query, SEED)?);
    }

    let mut index = LshIndex32::with_params(0.8, NUM_PERM, SEED, params)?;
    index.insert_many(
        references
            .iter()
            .enumerate()
            .map(|(key, sketch)| (key as u64, sketch)),
    )?;
    let candidates = index.query_many(&queries)?;
    let matches = candidates
        .iter()
        .enumerate()
        .filter(|(key, candidates)| candidates.binary_search(&(*key as u64)).is_ok())
        .count();
    Ok(matches as f64 / trials as f64)
}

fn next_u64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut value = *state;
    value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}
