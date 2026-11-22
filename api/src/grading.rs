//! Per-stat roll grades using Altair's midpoint bands.

/// Altair's chart uses signed percentage points from the center. A mild curse is favorable.
pub fn roll_grade(quality: f64, positive: bool) -> Option<(&'static str, f64)> {
    if !(0.0..=1.0).contains(&quality) {
        return None;
    }
    let delta = (quality * 20.0 - 10.0) * if positive { 1.0 } else { -1.0 };
    let bands = [
        (9.5, "S"),
        (7.5, "A+"),
        (5.5, "A"),
        (3.5, "A-"),
        (1.5, "B+"),
        (-1.5, "B"),
        (-3.5, "B-"),
        (-5.5, "C+"),
        (-7.5, "C"),
        (-9.5, "C-"),
    ];
    let letter = bands
        .iter()
        .find(|(floor, _)| delta >= *floor)
        .map_or("F", |(_, grade)| *grade);
    Some((letter, delta))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn midpoint_bands_and_curse_direction() {
        for (q, expected) in [
            (0.0, "F"),
            (0.05, "C-"),
            (0.15, "C"),
            (0.25, "C+"),
            (0.35, "B-"),
            (0.5, "B"),
            (0.575, "B+"),
            (0.675, "A-"),
            (0.775, "A"),
            (0.875, "A+"),
            (0.975, "S"),
            (1.0, "S"),
        ] {
            assert_eq!(roll_grade(q, true).unwrap().0, expected);
        }
        assert_eq!(roll_grade(0.0, false), Some(("S", 10.0)));
        assert_eq!(roll_grade(1.0, false), Some(("F", -10.0)));
        assert!(roll_grade(f64::NAN, true).is_none());
    }
}
