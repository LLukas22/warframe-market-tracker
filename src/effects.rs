//! Estimates how much each stat moves a riven's price: ridge regression of log price on which
//! stats a riven has. Log prices make effects multiplicative, so "+100%" means double. The ridge
//! penalty keeps rarely seen and overlapping stats from getting extreme estimates.

/// Tuned on real listings for a stable ranking: of the values that keep most weapons above the
/// minimum bar, this agreed best between two separate halves of a weapon's rivens.
const RIDGE: f64 = 10.0;
const FOLDS: usize = 5;

pub struct Fit {
    /// Change in log price from having each stat.
    pub coefficients: Vec<f64>,
    pub standard_errors: Vec<f64>,
    /// False when this coefficient can trade off with another or the intercept.
    pub identifiable: Vec<bool>,
    intercept: f64,
    means: Vec<f64>,
}

impl Fit {
    fn predict(&self, row: &[f64]) -> f64 {
        let centered = row.iter().zip(&self.means).map(|(x, m)| x - m);
        self.intercept
            + centered
                .zip(&self.coefficients)
                .map(|(x, b)| x * b)
                .sum::<f64>()
    }
}

fn invert(mut a: Vec<Vec<f64>>) -> Option<Vec<Vec<f64>>> {
    let n = a.len();
    let mut inverse: Vec<Vec<f64>> = (0..n)
        .map(|i| (0..n).map(|j| f64::from(u8::from(i == j))).collect())
        .collect();
    for col in 0..n {
        let pivot = (col..n).max_by(|&i, &j| a[i][col].abs().total_cmp(&a[j][col].abs()))?;
        if a[pivot][col].abs() < 1e-12 {
            return None;
        }
        a.swap(col, pivot);
        inverse.swap(col, pivot);
        let scale = a[col][col];
        a[col].iter_mut().for_each(|v| *v /= scale);
        inverse[col].iter_mut().for_each(|v| *v /= scale);
        for row in (0..n).filter(|&r| r != col) {
            let factor = a[row][col];
            for k in 0..n {
                a[row][k] -= factor * a[col][k];
                inverse[row][k] -= factor * inverse[col][k];
            }
        }
    }
    Some(inverse)
}

/// A ridge penalty makes a singular matrix invertible but cannot identify separate
/// effects. Find the null space of the unpenalized centered design's Gram matrix:
/// any coefficient participating in a null direction is not individually identifiable.
fn identifiable(mut gram: Vec<Vec<f64>>) -> Vec<bool> {
    let p = gram.len();
    let scale = gram.iter().flatten().fold(0.0f64, |a, b| a.max(b.abs()));
    let tolerance = scale.max(1.0) * 1e-9;
    let mut pivots = Vec::new();
    for col in 0..p {
        let row = pivots.len();
        let Some(pivot) =
            (row..p).max_by(|&i, &j| gram[i][col].abs().total_cmp(&gram[j][col].abs()))
        else {
            break;
        };
        if gram[pivot][col].abs() <= tolerance {
            continue;
        }
        gram.swap(row, pivot);
        let divisor = gram[row][col];
        for value in &mut gram[row] {
            *value /= divisor;
        }
        for other in 0..p {
            if other == row {
                continue;
            }
            let factor = gram[other][col];
            for k in 0..p {
                gram[other][k] -= factor * gram[row][k];
            }
        }
        pivots.push(col);
    }
    let free: Vec<usize> = (0..p).filter(|c| !pivots.contains(c)).collect();
    let mut result = vec![false; p];
    for (row, col) in pivots.into_iter().enumerate() {
        result[col] = free.iter().all(|&j| gram[row][j].abs() <= 1e-8);
    }
    result
}

pub fn fit(x: &[Vec<f64>], y: &[f64]) -> Option<Fit> {
    let (n, p) = (x.len(), x.first()?.len());
    let means: Vec<f64> = (0..p)
        .map(|j| x.iter().map(|row| row[j]).sum::<f64>() / n as f64)
        .collect();
    let intercept = y.iter().sum::<f64>() / n as f64;

    let mut gram = vec![vec![0.0; p]; p];
    let mut moment = vec![0.0; p];
    for (row, target) in x.iter().zip(y) {
        for i in 0..p {
            let xi = row[i] - means[i];
            moment[i] += xi * (target - intercept);
            for j in 0..p {
                gram[i][j] += xi * (row[j] - means[j]);
            }
        }
    }
    let mut penalized = gram.clone();
    (0..p).for_each(|i| penalized[i][i] += RIDGE);
    let inverse = invert(penalized)?;

    let product = |a: &[Vec<f64>], b: &[Vec<f64>]| -> Vec<Vec<f64>> {
        (0..p)
            .map(|i| {
                (0..p)
                    .map(|j| (0..p).map(|k| a[i][k] * b[k][j]).sum())
                    .collect()
            })
            .collect()
    };
    let coefficients: Vec<f64> = inverse
        .iter()
        .map(|row| row.iter().zip(&moment).map(|(a, m)| a * m).sum())
        .collect();
    let mut result = Fit {
        coefficients,
        standard_errors: Vec::new(),
        identifiable: identifiable(gram.clone()),
        intercept,
        means,
    };

    let hat = product(&inverse, &gram);
    let effective_parameters: f64 = (0..p).map(|i| hat[i][i]).sum();
    let residuals: f64 = x
        .iter()
        .zip(y)
        .map(|(row, target)| (target - result.predict(row)).powi(2))
        .sum();
    let variance = residuals / (n as f64 - effective_parameters).max(1.0);
    let covariance = product(&hat, &inverse);
    result.standard_errors = (0..p)
        .map(|i| (variance * covariance[i][i]).max(0.0).sqrt())
        .collect();
    Some(result)
}

/// How much of the price differences the stats explain on rivens left out of the fit
/// (cross-validated R²). Around 0 or below means the stats tell us nothing about the price.
pub fn holdout_r2(x: &[Vec<f64>], y: &[f64]) -> Option<f64> {
    let (mut missed, mut total) = (0.0, 0.0);
    for fold in 0..FOLDS {
        let train = |i: &usize| i % FOLDS != fold;
        let train_x: Vec<Vec<f64>> = (0..x.len()).filter(train).map(|i| x[i].clone()).collect();
        let train_y: Vec<f64> = (0..y.len()).filter(train).map(|i| y[i]).collect();
        let model = fit(&train_x, &train_y)?;
        let baseline = train_y.iter().sum::<f64>() / train_y.len() as f64;
        for i in (0..x.len()).filter(|i| i % FOLDS == fold) {
            missed += (y[i] - model.predict(&x[i])).powi(2);
            total += (y[i] - baseline).powi(2);
        }
    }
    (total > 0.0).then(|| 1.0 - missed / total)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo random numbers in 0..1, so tests never flake.
    fn noise(i: usize) -> f64 {
        ((i as f64 * 12.9898).sin() * 43758.5453).rem_euclid(1.0)
    }

    fn dataset(strength: f64) -> (Vec<Vec<f64>>, Vec<f64>) {
        (0..300)
            .map(|i| {
                let row = vec![
                    f64::from(noise(i) < 0.3),
                    f64::from(noise(i + 1000) < 0.3),
                    f64::from(noise(i + 2000) < 0.3),
                ];
                let y = 3.0
                    + strength * (row[0] * 2f64.ln() - row[1] * 2f64.ln())
                    + (noise(i + 3000) - 0.5);
                (row, y)
            })
            .unzip()
    }

    #[test]
    fn recovers_known_effects_and_their_direction() {
        let (x, y) = dataset(1.0);
        let model = fit(&x, &y).unwrap();
        let effect = |i: usize| model.coefficients[i].exp();
        // The penalty pulls estimates towards "no effect", but never past the true value.
        assert!((1.5..2.0).contains(&effect(0)), "doubles: {}", effect(0));
        assert!((0.5..0.7).contains(&effect(1)), "halves: {}", effect(1));
        assert!(model.coefficients[2].abs() < 2.0 * model.standard_errors[2]);
        assert!(model.coefficients[0].abs() > 2.0 * model.standard_errors[0]);
        assert!(holdout_r2(&x, &y).unwrap() > 0.3);
    }

    #[test]
    fn prices_unrelated_to_stats_explain_nothing() {
        let (x, y) = dataset(0.0);
        assert!(holdout_r2(&x, &y).unwrap() < 0.05);
    }

    #[test]
    fn identifiability_detects_aliases_constants_and_linear_combinations() {
        let x: Vec<Vec<f64>> = (0..100)
            .map(|i| {
                let a = (i % 2) as f64;
                let b = ((i / 2) % 2) as f64;
                let independent = ((i / 4) % 2) as f64;
                vec![a, b, a + b, 1.0, independent]
            })
            .collect();
        let y: Vec<f64> = x.iter().map(|r| 3.0 + r[0] + r[4]).collect();
        assert_eq!(
            fit(&x, &y).unwrap().identifiable,
            vec![false, false, false, false, true]
        );
    }

    #[test]
    fn overlapping_stats_stay_finite() {
        // Two stats that always come together can't be told apart; the penalty splits the effect.
        let x: Vec<Vec<f64>> = (0..40).map(|i| vec![f64::from(i % 2); 2]).collect();
        let y: Vec<f64> = (0..40).map(|i| 3.0 + f64::from(i % 2)).collect();
        let model = fit(&x, &y).unwrap();
        assert!(model.coefficients.iter().all(|b| b.is_finite() && *b > 0.0));
        assert_eq!(model.identifiable, vec![false, false]);
        assert!((model.coefficients[0] - model.coefficients[1]).abs() < 1e-9);
    }
}
