//! Small dense least squares (Householder QR): fitting a correction's radial curve with the polynomial basis the
//! develop pipeline evaluates. QR instead of normal equations: polynomial bases on `[0, 1]` are ill-conditioned and
//! squaring their condition number would lose half the digits.

/// Coefficients `x` minimising `‖A·x − b‖` for the `rows × cols` matrix `a` (row-major), or `None` when the basis is
/// rank deficient, the sizes don't agree or a value isn't finite.
pub fn least_squares(a: &[f64], b: &[f64], rows: usize, cols: usize) -> Option<Vec<f64>> {
    if cols == 0 || rows < cols || a.len() != rows.checked_mul(cols)? || b.len() != rows {
        return None;
    }
    if a.iter().chain(b).any(|v| !v.is_finite()) {
        return None;
    }
    let mut m = a.to_vec();
    let mut y = b.to_vec();
    let at = |r: usize, c: usize| r * cols + c;
    for k in 0..cols {
        // the Householder reflection that zeroes column k below the diagonal
        let norm = (k..rows).map(|r| m.get(at(r, k)).copied().unwrap_or(0.0).powi(2)).sum::<f64>().sqrt();
        if norm < 1e-300 {
            return None;
        }
        let akk = m.get(at(k, k)).copied().unwrap_or(0.0);
        let alpha = if akk > 0.0 { -norm } else { norm };
        let mut v: Vec<f64> = (k..rows).map(|r| m.get(at(r, k)).copied().unwrap_or(0.0)).collect();
        if let Some(v0) = v.first_mut() {
            *v0 -= alpha;
        }
        let vv: f64 = v.iter().map(|x| x * x).sum();
        if vv < 1e-300 {
            continue;
        }
        for c in k..cols {
            let dot: f64 = v.iter().enumerate().map(|(i, vi)| vi * m.get(at(k + i, c)).copied().unwrap_or(0.0)).sum();
            let f = 2.0 * dot / vv;
            for (i, vi) in v.iter().enumerate() {
                if let Some(x) = m.get_mut(at(k + i, c)) {
                    *x -= f * vi;
                }
            }
        }
        let dot: f64 = v.iter().enumerate().map(|(i, vi)| vi * y.get(k + i).copied().unwrap_or(0.0)).sum();
        let f = 2.0 * dot / vv;
        for (i, vi) in v.iter().enumerate() {
            if let Some(x) = y.get_mut(k + i) {
                *x -= f * vi;
            }
        }
    }
    // back substitution on the upper triangle
    let mut x = vec![0.0; cols];
    for k in (0..cols).rev() {
        let diag = m.get(at(k, k)).copied().unwrap_or(0.0);
        if diag.abs() < 1e-14 {
            return None;
        }
        let s: f64 = ((k + 1)..cols).map(|c| m.get(at(k, c)).copied().unwrap_or(0.0) * x.get(c).copied().unwrap_or(0.0)).sum();
        let v = (y.get(k).copied().unwrap_or(0.0) - s) / diag;
        if let Some(xk) = x.get_mut(k) {
            *xk = v;
        }
    }
    x.iter().all(|v| v.is_finite()).then_some(x)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovers_an_exact_polynomial() {
        let truth = [1.0, -0.02, 0.05, 0.003, -0.01, 0.0004, 0.002];
        let (mut a, mut b) = (Vec::new(), Vec::new());
        for i in 0..64 {
            let r = i as f64 / 63.0;
            let row: Vec<f64> = (0..7).map(|p| r.powi(p)).collect();
            b.push(row.iter().zip(truth).map(|(x, t)| x * t).sum());
            a.extend(row);
        }
        let x = least_squares(&a, &b, 64, 7).unwrap();
        for (got, want) in x.iter().zip(truth) {
            assert!((got - want).abs() < 1e-9, "{x:?}");
        }
    }

    #[test]
    fn refuses_bad_shapes_and_values() {
        assert!(least_squares(&[1.0, 2.0], &[1.0], 1, 2).is_none(), "fewer rows than unknowns");
        assert!(least_squares(&[1.0, 2.0, 3.0], &[1.0, 2.0], 2, 2).is_none(), "wrong length");
        assert!(least_squares(&[1.0, f64::NAN], &[1.0, 1.0], 2, 1).is_none());
        assert!(least_squares(&[0.0, 0.0], &[1.0, 1.0], 2, 1).is_none(), "rank deficient");
    }
}
