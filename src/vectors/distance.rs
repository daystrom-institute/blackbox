use wide::f32x8;

pub fn cosine_distance(left: &[f32], right: &[f32]) -> f32 {
    debug_assert_eq!(left.len(), right.len());
    1.0 - dot(left, right)
}

pub fn dot(left: &[f32], right: &[f32]) -> f32 {
    debug_assert_eq!(left.len(), right.len());

    let mut sum = f32x8::ZERO;
    let mut left_chunks = left.chunks_exact(8);
    let mut right_chunks = right.chunks_exact(8);

    for (left_chunk, right_chunk) in left_chunks.by_ref().zip(right_chunks.by_ref()) {
        sum += f32x8::from([
            left_chunk[0],
            left_chunk[1],
            left_chunk[2],
            left_chunk[3],
            left_chunk[4],
            left_chunk[5],
            left_chunk[6],
            left_chunk[7],
        ]) * f32x8::from([
            right_chunk[0],
            right_chunk[1],
            right_chunk[2],
            right_chunk[3],
            right_chunk[4],
            right_chunk[5],
            right_chunk[6],
            right_chunk[7],
        ]);
    }

    sum.reduce_add()
        + left_chunks
            .remainder()
            .iter()
            .zip(right_chunks.remainder())
            .map(|(left, right)| left * right)
            .sum::<f32>()
}

pub fn scalar_cosine_distance(left: &[f32], right: &[f32]) -> f32 {
    debug_assert_eq!(left.len(), right.len());
    1.0 - left
        .iter()
        .zip(right)
        .map(|(left, right)| left * right)
        .sum::<f32>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simd_cosine_matches_scalar_reference() {
        let left = (0..103)
            .map(|idx| ((idx as f32) * 0.013).sin())
            .collect::<Vec<_>>();
        let right = (0..103)
            .map(|idx| ((idx as f32) * 0.017).cos())
            .collect::<Vec<_>>();
        let simd = cosine_distance(&left, &right);
        let scalar = scalar_cosine_distance(&left, &right);
        assert!((simd - scalar).abs() <= 1.0e-6, "simd={simd} scalar={scalar}");
    }
}
