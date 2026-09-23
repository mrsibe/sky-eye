use crate::{
    astrometry::wcs::{SipDistortion, Wcs},
    catalog::vizier::GaiaSource,
    reduction::SourceMeasurement,
};
use delaunator::{triangulate, Point as DelaunayPoint};
use nalgebra::{DMatrix, DVector, Matrix2, Matrix3, Vector2, Vector3};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Plate model requested for the astrometric solution.
///
/// `linear` is the shipped default and must reproduce the historical
/// 6-parameter affine + TAN/CD solution exactly (no SIP). `quadratic` and
/// `cubic` enable SIP distortion terms of the corresponding order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlateModel {
    #[default]
    Linear,
    Quadratic,
    Cubic,
}

impl PlateModel {
    /// Polynomial degree used by the SIP data path. Linear maps to order 1,
    /// which contains no SIP terms (constant/linear terms are excluded).
    pub fn order(self) -> usize {
        match self {
            Self::Linear => 1,
            Self::Quadratic => 2,
            Self::Cubic => 3,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Linear => "linear",
            Self::Quadratic => "quadratic",
            Self::Cubic => "cubic",
        }
    }

    fn from_order(order: usize) -> Self {
        match order {
            2 => Self::Quadratic,
            3 => Self::Cubic,
            _ => Self::Linear,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MatchConfig {
    pub max_image_sources: usize,
    pub max_catalog_sources: usize,
    pub invariant_tolerance: f64,
    pub max_candidate_evaluations: usize,
    pub minimum_seed_matches: usize,
    pub minimum_matches: usize,
    pub maximum_rms_arcsec: f64,
    pub pixel_scale_hint_arcsec: Option<f64>,
    pub rotation_hint_deg: Option<f64>,
    pub parity_hint: Option<bool>,
    pub scale_tolerance_fraction: f64,
    pub catalog_bright_limit_mag: Option<f32>,
    pub catalog_faint_limit_mag: Option<f32>,
    pub plate_model: PlateModel,
    /// Fraction of validated matches held out for SIP cross-validation.
    pub sip_holdout_fraction: f64,
    /// Required relative holdout-RMS improvement for a SIP order to be accepted.
    pub sip_holdout_improvement_fraction: f64,
}

impl Default for MatchConfig {
    fn default() -> Self {
        Self {
            max_image_sources: 60,
            max_catalog_sources: 256,
            invariant_tolerance: 0.008,
            max_candidate_evaluations: 25_000,
            minimum_seed_matches: 4,
            minimum_matches: 8,
            maximum_rms_arcsec: 3.0,
            pixel_scale_hint_arcsec: None,
            rotation_hint_deg: None,
            parity_hint: None,
            scale_tolerance_fraction: 0.08,
            catalog_bright_limit_mag: None,
            catalog_faint_limit_mag: None,
            plate_model: PlateModel::Linear,
            sip_holdout_fraction: 0.30,
            sip_holdout_improvement_fraction: 0.25,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AstrometricMatch {
    pub image_source_index: usize,
    pub catalog_source_index: usize,
    pub residual_arcsec: f64,
    pub residual_x_arcsec: f64,
    pub residual_y_arcsec: f64,
    pub weight: f64,
    pub used: bool,
    pub rejection_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AstrometricSolution {
    pub wcs: Wcs,
    pub matches: Vec<AstrometricMatch>,
    pub rms_arcsec: f64,
    pub plate_model_requested: PlateModel,
    pub plate_model_applied: PlateModel,
    pub sip_order: Option<usize>,
    pub sip_escalation_attempted: bool,
    pub sip_escalation_succeeded: bool,
    /// `None` when no SIP fit was attempted; otherwise whether the accepted or
    /// last-tried candidate order converged. A non-converged fit is rejected
    /// rather than silently shipped.
    pub sip_fit_converged: Option<bool>,
    pub plate_model_downgraded: bool,
    pub plate_model_downgrade_reason: Option<String>,
    pub sip_rejection_reason: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum MatchError {
    #[error("not enough quality image sources: found {found}, need at least {required}")]
    InsufficientImageSources { found: usize, required: usize },
    #[error("not enough Gaia reference sources: found {found}, need at least {required}")]
    InsufficientCatalogSources { found: usize, required: usize },
    #[error("no pair-voting or triangle pattern produced a consistent astrometric candidate")]
    NoPattern,
    #[error(
        "best initial candidate has only {found} seed matches; at least {required} are required"
    )]
    InsufficientSeedMatches { found: usize, required: usize },
    #[error(
        "refined solution has only {found} validated matches; at least {required} are required"
    )]
    InsufficientFinalMatches { found: usize, required: usize },
    #[error("astrometric fit is singular")]
    SingularFit,
    #[error("astrometric RMS {rms_arcsec:.3} arcsec exceeds limit {limit_arcsec:.3} arcsec")]
    ExcessiveResidual { rms_arcsec: f64, limit_arcsec: f64 },
}

#[derive(Debug, Clone, Copy)]
struct Point {
    original_index: usize,
    x: f64,
    y: f64,
    /// Image points store pixels; catalogue points store tangent-plane degrees.
    sigma_native: f64,
}

#[derive(Debug, Clone, Copy)]
struct Triangle {
    indices: [usize; 3],
    short_ratio: f64,
    middle_ratio: f64,
}

#[derive(Debug, Clone, Copy)]
struct Affine {
    x_x: f64,
    x_y: f64,
    x_0: f64,
    y_x: f64,
    y_y: f64,
    y_0: f64,
}

impl Affine {
    fn matrix(self) -> Matrix2<f64> {
        Matrix2::new(self.x_x, self.x_y, self.y_x, self.y_y)
    }

    fn apply(self, point: Point) -> Vector2<f64> {
        Vector2::new(
            self.x_x * point.x + self.x_y * point.y + self.x_0,
            self.y_x * point.x + self.y_y * point.y + self.y_0,
        )
    }

    fn pixel_scale_arcsec(self) -> f64 {
        (self.x_x * self.y_y - self.x_y * self.y_x).abs().sqrt() * 3_600.0
    }

    fn rotation_deg(self) -> f64 {
        self.y_x.atan2(self.x_x).to_degrees()
    }
}

#[derive(Debug, Clone, Copy)]
struct Pair {
    first: usize,
    second: usize,
    distance: f64,
}

#[derive(Debug, Clone, Copy)]
struct HoughVote {
    count: u32,
    sum: [f64; 6],
}

impl HoughVote {
    fn new(affine: Affine) -> Self {
        Self {
            count: 1,
            sum: [
                affine.x_x, affine.x_y, affine.x_0, affine.y_x, affine.y_y, affine.y_0,
            ],
        }
    }

    fn add(&mut self, affine: Affine) {
        self.count += 1;
        for (sum, value) in self.sum.iter_mut().zip([
            affine.x_x, affine.x_y, affine.x_0, affine.y_x, affine.y_y, affine.y_0,
        ]) {
            *sum += value;
        }
    }

    fn average(self) -> Affine {
        let divisor = f64::from(self.count);
        Affine {
            x_x: self.sum[0] / divisor,
            x_y: self.sum[1] / divisor,
            x_0: self.sum[2] / divisor,
            y_x: self.sum[3] / divisor,
            y_y: self.sum[4] / divisor,
            y_0: self.sum[5] / divisor,
        }
    }
}

pub fn solve_near_field(
    image_sources: &[SourceMeasurement],
    catalog_sources: &[GaiaSource],
    center_ra_deg: f64,
    center_dec_deg: f64,
    image_width: u32,
    image_height: u32,
    config: MatchConfig,
) -> Result<AstrometricSolution, MatchError> {
    let image_points = quality_image_points(image_sources, image_width, image_height, &config);
    if image_points.len() < config.minimum_matches {
        return Err(MatchError::InsufficientImageSources {
            found: image_points.len(),
            required: config.minimum_matches,
        });
    }
    let catalog_points =
        quality_catalog_points(catalog_sources, center_ra_deg, center_dec_deg, &config);
    if catalog_points.len() < config.minimum_matches {
        return Err(MatchError::InsufficientCatalogSources {
            found: catalog_points.len(),
            required: config.minimum_matches,
        });
    }
    log::debug!(
        "[sky-eye][matcher] quality image sources: {}/{}, Gaia sources: {}/{}, candidate limit: {}",
        image_points.len(),
        image_sources.len(),
        catalog_points.len(),
        catalog_sources.len(),
        config.max_candidate_evaluations
    );
    log::debug!(
        "[sky-eye][matcher] active hints: scale={:?} arcsec/px rotation={:?} deg parity={:?}",
        config.pixel_scale_hint_arcsec,
        config.rotation_hint_deg,
        config.parity_hint
    );

    let hinted = config.pixel_scale_hint_arcsec.and_then(|scale_hint| {
        pair_vote_candidate(
            &image_points,
            &catalog_points,
            image_width,
            image_height,
            scale_hint,
            &config,
        )
    });
    if let Some((_, matches)) = &hinted {
        log::debug!(
            "[sky-eye][matcher] pair-voting initial match count: {}",
            matches.len()
        );
    }

    let mut evaluated = 0usize;
    let mut best: Option<(Affine, Vec<AstrometricMatch>)> = hinted;
    if best
        .as_ref()
        .is_none_or(|(_, matches)| matches.len() < config.minimum_matches)
    {
        let image_triangles = triangles(&image_points);
        let permutations = [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ];

        let mut tier_sizes = vec![
            image_points.len(),
            image_points.len() * 3 / 2,
            image_points.len() * 2,
            catalog_points.len(),
        ];
        tier_sizes.iter_mut().for_each(|size| {
            *size = (*size)
                .min(catalog_points.len())
                .max(config.minimum_matches)
        });
        tier_sizes.sort_unstable();
        tier_sizes.dedup();

        'search: for tier_size in tier_sizes {
            let catalog_tier = &catalog_points[..tier_size];
            let catalog_triangles = triangles(catalog_tier);
            let triangle_pairs = symmetric_triangle_pairs(
                &image_triangles,
                &catalog_triangles,
                config.invariant_tolerance,
            );
            log::debug!(
                "[sky-eye][matcher] extended Delaunay tier: Gaia={} image_triangles={} catalog_triangles={} symmetric_pairs={}",
                tier_size,
                image_triangles.len(),
                catalog_triangles.len(),
                triangle_pairs.len()
            );
            for (image_triangle, catalog_triangle) in triangle_pairs {
                for permutation in permutations {
                    evaluated += 1;
                    if evaluated > config.max_candidate_evaluations {
                        break 'search;
                    }
                    let image_seed = image_triangle.indices.map(|index| image_points[index]);
                    let catalog_seed = [
                        catalog_tier[catalog_triangle.indices[permutation[0]]],
                        catalog_tier[catalog_triangle.indices[permutation[1]]],
                        catalog_tier[catalog_triangle.indices[permutation[2]]],
                    ];
                    let Some(affine) = affine_from_three(image_seed, catalog_seed) else {
                        continue;
                    };
                    let scale = affine.pixel_scale_arcsec();
                    if !scale.is_finite()
                        || !(0.05..=20.0).contains(&scale)
                        || !is_near_similarity(affine)
                    {
                        continue;
                    }
                    // Rotation and parity hints may be stale instrument metadata.
                    // The Delaunay path searches both orientations and relies on
                    // full-field validation instead of rejecting a valid seed.
                    let tolerance = (scale * 5.0).clamp(2.0, 8.0);
                    let matches = associate(affine, &image_points, &catalog_points, tolerance);
                    let replace = best.as_ref().is_none_or(|(_, current)| {
                        matches.len() > current.len()
                            || (matches.len() == current.len() && rms(&matches) < rms(current))
                    });
                    if replace {
                        best = Some((affine, matches));
                    }
                }
            }
        }
    }

    let (_, mut matches) = best.ok_or(MatchError::NoPattern)?;
    log::debug!(
        "[sky-eye][matcher] evaluated {evaluated} triangle candidates; best initial match count: {}",
        matches.len()
    );
    if matches.len() < config.minimum_seed_matches {
        return Err(MatchError::InsufficientSeedMatches {
            found: matches.len(),
            required: config.minimum_seed_matches,
        });
    }

    for _ in 0..5 {
        let candidate_affine = fit_affine(&image_points, &catalog_points, &matches)?;
        let scale = candidate_affine.pixel_scale_arcsec();
        let broad_tolerance = (scale * 4.0).clamp(1.5, 10.0);
        let candidates = associate(
            candidate_affine,
            &image_points,
            &catalog_points,
            broad_tolerance,
        );
        if candidates.len() < config.minimum_seed_matches {
            break;
        }
        let candidate_rms = rms(&candidates);
        let clip = (candidate_rms * 2.8).clamp(0.8, broad_tolerance);
        let clipped: Vec<_> = candidates
            .into_iter()
            .filter(|pair| pair.residual_arcsec <= clip)
            .collect();
        if clipped.len() < config.minimum_seed_matches {
            break;
        }
        let unchanged = clipped.len() == matches.len()
            && clipped.iter().zip(&matches).all(|(left, right)| {
                left.image_source_index == right.image_source_index
                    && left.catalog_source_index == right.catalog_source_index
            });
        matches = clipped;
        if unchanged {
            break;
        }
    }
    let affine = fit_affine(&image_points, &catalog_points, &matches)?;
    matches = associate(
        affine,
        &image_points,
        &catalog_points,
        (affine.pixel_scale_arcsec() * 2.5).clamp(0.8, 5.0),
    );
    if matches.len() < config.minimum_matches {
        return Err(MatchError::InsufficientFinalMatches {
            found: matches.len(),
            required: config.minimum_matches,
        });
    }
    let solution = finish_with_plate_model(
        &image_points,
        &catalog_points,
        affine,
        matches,
        &config,
        center_ra_deg,
        center_dec_deg,
        image_width,
        image_height,
    )?;
    if solution.rms_arcsec > config.maximum_rms_arcsec {
        return Err(MatchError::ExcessiveResidual {
            rms_arcsec: solution.rms_arcsec,
            limit_arcsec: config.maximum_rms_arcsec,
        });
    }
    Ok(solution)
}

pub fn refine_from_wcs_seed(
    image_sources: &[SourceMeasurement],
    catalog_sources: &[GaiaSource],
    seed: Wcs,
    config: MatchConfig,
) -> Result<AstrometricSolution, MatchError> {
    let image_points =
        quality_image_points(image_sources, seed.image_width, seed.image_height, &config);
    let catalog_points = quality_catalog_points(catalog_sources, seed.crval1, seed.crval2, &config);
    let mut affine = Affine {
        x_x: seed.cd1_1,
        x_y: seed.cd1_2,
        x_0: -seed.cd1_1 * seed.crpix1 - seed.cd1_2 * seed.crpix2,
        y_x: seed.cd2_1,
        y_y: seed.cd2_2,
        y_0: -seed.cd2_1 * seed.crpix1 - seed.cd2_2 * seed.crpix2,
    };
    let scale = affine.pixel_scale_arcsec();
    let mut matches = associate(
        affine,
        &image_points,
        &catalog_points,
        (scale * 25.0).clamp(4.0, 12.0),
    );
    if matches.len() < config.minimum_seed_matches {
        return Err(MatchError::InsufficientSeedMatches {
            found: matches.len(),
            required: config.minimum_seed_matches,
        });
    }

    for iteration in 0..6 {
        affine = fit_affine(&image_points, &catalog_points, &matches)?;
        let tolerance_pixels = if iteration == 0 { 12.0 } else { 6.0 };
        let tolerance = (affine.pixel_scale_arcsec() * tolerance_pixels).clamp(1.0, 8.0);
        let candidates = associate(affine, &image_points, &catalog_points, tolerance);
        if candidates.len() < config.minimum_seed_matches {
            break;
        }
        let candidate_rms = rms(&candidates);
        let clip = (candidate_rms * 2.8).clamp(0.6, tolerance);
        let clipped: Vec<_> = candidates
            .into_iter()
            .filter(|pair| pair.residual_arcsec <= clip)
            .collect();
        if clipped.len() < config.minimum_seed_matches {
            break;
        }
        matches = clipped;
    }

    affine = fit_affine(&image_points, &catalog_points, &matches)?;
    matches = associate(
        affine,
        &image_points,
        &catalog_points,
        (affine.pixel_scale_arcsec() * 4.0).clamp(0.8, 4.0),
    );
    if matches.len() < config.minimum_matches {
        return Err(MatchError::InsufficientFinalMatches {
            found: matches.len(),
            required: config.minimum_matches,
        });
    }
    affine = fit_affine(&image_points, &catalog_points, &matches)?;
    let solution = finish_with_plate_model(
        &image_points,
        &catalog_points,
        affine,
        matches,
        &config,
        seed.crval1,
        seed.crval2,
        seed.image_width,
        seed.image_height,
    )?;
    if solution.rms_arcsec > config.maximum_rms_arcsec {
        return Err(MatchError::ExcessiveResidual {
            rms_arcsec: solution.rms_arcsec,
            limit_arcsec: config.maximum_rms_arcsec,
        });
    }
    Ok(solution)
}

fn pair_vote_candidate(
    image_points: &[Point],
    catalog_points: &[Point],
    image_width: u32,
    image_height: u32,
    scale_hint_arcsec: f64,
    config: &MatchConfig,
) -> Option<(Affine, Vec<AstrometricMatch>)> {
    if !scale_hint_arcsec.is_finite() || !(0.05..=20.0).contains(&scale_hint_arcsec) {
        return None;
    }
    let image_pairs = pairs(image_points, 25.0);
    let catalog_pairs = pairs(catalog_points, scale_hint_arcsec * 25.0 / 3_600.0);
    let translation_bin_deg = (scale_hint_arcsec * 12.0 / 3_600.0).max(0.000_2);
    let rotation_bin_deg = 1.0;
    let scale_bin_fraction = 0.01;
    let tolerance = config.scale_tolerance_fraction.clamp(0.01, 0.5);
    let mut votes: HashMap<(i32, i32, i32, i32, bool), HoughVote> = HashMap::new();
    let mut hypotheses = 0usize;
    let image_center = Point {
        original_index: 0,
        x: f64::from(image_width) * 0.5,
        y: f64::from(image_height) * 0.5,
        sigma_native: 0.1,
    };

    let parities: &[bool] = match config.parity_hint {
        Some(true) => &[true],
        Some(false) => &[false],
        None => &[false, true],
    };
    let variants_per_pair = 2 * parities.len();
    let catalog_budget_per_image_pair =
        (1_000_000 / image_pairs.len().max(1) / variants_per_pair).max(1);

    // Give every image pair an equal hypothesis budget. The previous nested-loop
    // cap exhausted one million trials on only the first few (longest) image
    // pairs, so most of the field could never vote for the correct transform.
    for image_pair in &image_pairs {
        let compatible_catalog_pairs: Vec<&Pair> = catalog_pairs
            .iter()
            .filter(|catalog_pair| {
                let scale = catalog_pair.distance * 3_600.0 / image_pair.distance;
                ((scale / scale_hint_arcsec) - 1.0).abs() <= tolerance
            })
            .collect();
        let sample_count = compatible_catalog_pairs
            .len()
            .min(catalog_budget_per_image_pair);
        for sample_index in 0..sample_count {
            let catalog_pair = compatible_catalog_pairs
                [sample_index * compatible_catalog_pairs.len() / sample_count];
            let scale = catalog_pair.distance * 3_600.0 / image_pair.distance;
            for reverse in [false, true] {
                let (catalog_first, catalog_second) = if reverse {
                    (
                        catalog_points[catalog_pair.second],
                        catalog_points[catalog_pair.first],
                    )
                } else {
                    (
                        catalog_points[catalog_pair.first],
                        catalog_points[catalog_pair.second],
                    )
                };
                for &flipped in parities {
                    hypotheses += 1;
                    let affine = similarity_from_pair(
                        image_points[image_pair.first],
                        image_points[image_pair.second],
                        catalog_first,
                        catalog_second,
                        flipped,
                    )?;
                    if let Some(rotation_hint) = config.rotation_hint_deg {
                        if angle_difference_deg(affine.rotation_deg(), rotation_hint).abs() > 25.0 {
                            continue;
                        }
                    }
                    let projected_center = affine.apply(image_center);
                    let key = (
                        (normalize_angle_deg(affine.rotation_deg()) / rotation_bin_deg).round()
                            as i32,
                        ((scale / scale_hint_arcsec).ln() / scale_bin_fraction).round() as i32,
                        (projected_center.x / translation_bin_deg).round() as i32,
                        (projected_center.y / translation_bin_deg).round() as i32,
                        flipped,
                    );
                    votes
                        .entry(key)
                        .and_modify(|vote| vote.add(affine))
                        .or_insert_with(|| HoughVote::new(affine));
                }
            }
        }
    }

    let mut ranked: Vec<_> = votes.into_values().collect();
    ranked.sort_by_key(|vote| std::cmp::Reverse(vote.count));
    log::debug!(
        "[sky-eye][matcher] pair voting evaluated {hypotheses} hypotheses in {} bins; top vote count: {}",
        ranked.len(),
        ranked.first().map_or(0, |vote| vote.count)
    );
    let mut best: Option<(Affine, Vec<AstrometricMatch>)> = None;
    for vote in ranked.into_iter().take(512) {
        let affine = vote.average();
        let scale = affine.pixel_scale_arcsec();
        let matches = associate(
            affine,
            image_points,
            catalog_points,
            (scale * 10.0).clamp(4.0, 30.0),
        );
        let replace = best.as_ref().is_none_or(|(_, current)| {
            matches.len() > current.len()
                || (matches.len() == current.len() && rms(&matches) < rms(current))
        });
        if replace {
            best = Some((affine, matches));
        }
    }
    if let Some((affine, matches)) = &best {
        log::debug!(
            "[sky-eye][matcher] best pair model: scale={:.5} arcsec/px rotation={:.3} deg parity={} broad_matches={}",
            affine.pixel_scale_arcsec(),
            affine.rotation_deg(),
            if affine.x_x * affine.y_y - affine.x_y * affine.y_x < 0.0 {
                "flipped"
            } else {
                "normal"
            },
            matches.len()
        );
    }
    best
}

fn pairs(points: &[Point], minimum_distance: f64) -> Vec<Pair> {
    let mut result = Vec::new();
    for first in 0..points.len().saturating_sub(1) {
        for second in first + 1..points.len() {
            let distance = distance(points[first], points[second]);
            if distance >= minimum_distance {
                result.push(Pair {
                    first,
                    second,
                    distance,
                });
            }
        }
    }
    result.sort_by(|left, right| right.distance.total_cmp(&left.distance));
    result
}

fn similarity_from_pair(
    image_first: Point,
    image_second: Point,
    catalog_first: Point,
    catalog_second: Point,
    flipped: bool,
) -> Option<Affine> {
    let vx = image_second.x - image_first.x;
    let vy = image_second.y - image_first.y;
    let wx = catalog_second.x - catalog_first.x;
    let wy = catalog_second.y - catalog_first.y;
    let norm = vx * vx + vy * vy;
    if norm <= f64::EPSILON {
        return None;
    }
    let (x_x, x_y, y_x, y_y) = if flipped {
        let a = (wx * vx - wy * vy) / norm;
        let b = (wx * vy + wy * vx) / norm;
        (a, b, b, -a)
    } else {
        let a = (wx * vx + wy * vy) / norm;
        let b = (wy * vx - wx * vy) / norm;
        (a, -b, b, a)
    };
    Some(Affine {
        x_x,
        x_y,
        x_0: catalog_first.x - x_x * image_first.x - x_y * image_first.y,
        y_x,
        y_y,
        y_0: catalog_first.y - y_x * image_first.x - y_y * image_first.y,
    })
}

fn normalize_angle_deg(angle: f64) -> f64 {
    angle.rem_euclid(360.0)
}

fn angle_difference_deg(left: f64, right: f64) -> f64 {
    (left - right + 180.0).rem_euclid(360.0) - 180.0
}

fn is_near_similarity(affine: Affine) -> bool {
    let first_scale = affine.x_x.hypot(affine.y_x);
    let second_scale = affine.x_y.hypot(affine.y_y);
    if first_scale <= f64::EPSILON || second_scale <= f64::EPSILON {
        return false;
    }
    let scale_ratio = first_scale / second_scale;
    let cosine =
        (affine.x_x * affine.x_y + affine.y_x * affine.y_y).abs() / (first_scale * second_scale);
    (0.8..=1.25).contains(&scale_ratio) && cosine <= 0.2
}

fn quality_image_points(
    sources: &[SourceMeasurement],
    width: u32,
    height: u32,
    config: &MatchConfig,
) -> Vec<Point> {
    let mut candidates: Vec<_> = sources
        .iter()
        .enumerate()
        .filter(|(_, source)| {
            source.x.is_finite()
                && source.y.is_finite()
                && source.flux.is_finite()
                && source.flux > 0.0
                && source.x >= 3.0
                && source.y >= 3.0
                && source.x < (f64::from(width) - 3.0).max(0.0)
                && source.y < (f64::from(height) - 3.0).max(0.0)
                && source.fwhm > 0.4
                && source.fwhm < 30.0
                && source.ellipticity < 0.65
                && source.flags & sep_sys::SEP_OBJ_TRUNC == 0
                && !source.saturated
        })
        .collect();
    // SEP returns objects in extraction order, which is predominantly spatial.
    // Triangle matching needs a representative set of high-SNR stars instead
    // of an arbitrary strip near the first image rows.
    candidates.sort_by(|(_, left), (_, right)| right.flux.total_cmp(&left.flux));
    candidates
        .into_iter()
        .take(config.max_image_sources)
        .map(|(index, source)| Point {
            original_index: index,
            x: source.x,
            y: source.y,
            sigma_native: source
                .x_error_px
                .zip(source.y_error_px)
                .map(|(x, y)| x.hypot(y) / std::f64::consts::SQRT_2)
                .unwrap_or(0.25)
                .clamp(0.02, 2.0),
        })
        .collect()
}

fn quality_catalog_points(
    sources: &[GaiaSource],
    center_ra_deg: f64,
    center_dec_deg: f64,
    config: &MatchConfig,
) -> Vec<Point> {
    let mut ranked_sources: Vec<_> = sources
        .iter()
        .enumerate()
        .filter(|(_, source)| match source.g_mag {
            Some(magnitude) => {
                config
                    .catalog_bright_limit_mag
                    .is_none_or(|limit| magnitude >= limit)
                    && config
                        .catalog_faint_limit_mag
                        .is_none_or(|limit| magnitude <= limit)
            }
            None => {
                config.catalog_bright_limit_mag.is_none()
                    && config.catalog_faint_limit_mag.is_none()
            }
        })
        .filter(|(_, source)| {
            !source.duplicated_source
                && source.ruwe.is_none_or(|ruwe| ruwe <= 1.4)
                && source
                    .astrometric_params_solved
                    .is_none_or(|solved| solved == 31 || solved == 95)
        })
        .collect();
    ranked_sources.sort_by(|(_, left), (_, right)| {
        left.g_mag
            .unwrap_or(f32::INFINITY)
            .total_cmp(&right.g_mag.unwrap_or(f32::INFINITY))
    });
    ranked_sources
        .into_iter()
        .filter_map(|(index, source)| {
            let (x, y) =
                sky_to_tangent(source.ra_deg, source.dec_deg, center_ra_deg, center_dec_deg)?;
            Some(Point {
                original_index: index,
                x,
                y,
                sigma_native: source
                    .propagated_ra_error_mas
                    .unwrap_or(5.0)
                    .hypot(source.propagated_dec_error_mas.unwrap_or(5.0))
                    / std::f64::consts::SQRT_2
                    / 3_600_000.0,
            })
        })
        .take(config.max_catalog_sources)
        .collect()
}

fn sky_to_tangent(
    ra_deg: f64,
    dec_deg: f64,
    center_ra_deg: f64,
    center_dec_deg: f64,
) -> Option<(f64, f64)> {
    let ra = ra_deg.to_radians();
    let dec = dec_deg.to_radians();
    let ra0 = center_ra_deg.to_radians();
    let dec0 = center_dec_deg.to_radians();
    let delta_ra = ra - ra0;
    let denominator = dec.sin() * dec0.sin() + dec.cos() * dec0.cos() * delta_ra.cos();
    if denominator <= 0.0 {
        return None;
    }
    let x = dec.cos() * delta_ra.sin() / denominator;
    let y = (dec.sin() * dec0.cos() - dec.cos() * dec0.sin() * delta_ra.cos()) / denominator;
    Some((x.to_degrees(), y.to_degrees()))
}

fn triangles(points: &[Point]) -> Vec<Triangle> {
    if points.len() < 3 {
        return Vec::new();
    }
    let coordinates: Vec<_> = points
        .iter()
        .map(|point| DelaunayPoint {
            x: point.x,
            y: point.y,
        })
        .collect();
    let triangulation = triangulate(&coordinates);
    let mut neighbours = vec![HashSet::new(); points.len()];
    let mut index_sets = HashSet::new();
    for triangle in triangulation.triangles.as_chunks::<3>().0 {
        let mut indices = [triangle[0], triangle[1], triangle[2]];
        indices.sort_unstable();
        index_sets.insert(indices);
        for (left, right) in [
            (indices[0], indices[1]),
            (indices[0], indices[2]),
            (indices[1], indices[2]),
        ] {
            neighbours[left].insert(right);
            neighbours[right].insert(left);
        }
    }

    // Level-1 extension: combine each Delaunay vertex with pairs of its direct
    // neighbours. This remains O(N) for stellar point sets while tolerating
    // missing and spurious detections that change the base triangulation.
    for (center, adjacent) in neighbours.iter().enumerate() {
        let adjacent: Vec<_> = adjacent.iter().copied().collect();
        for first in 0..adjacent.len().saturating_sub(1) {
            for second in first + 1..adjacent.len() {
                let mut indices = [center, adjacent[first], adjacent[second]];
                indices.sort_unstable();
                index_sets.insert(indices);
            }
        }
    }

    index_sets
        .into_iter()
        .filter_map(|indices| triangle_from_indices(points, indices))
        .collect()
}

fn triangle_from_indices(points: &[Point], indices: [usize; 3]) -> Option<Triangle> {
    let a = distance(points[indices[0]], points[indices[1]]);
    let b = distance(points[indices[0]], points[indices[2]]);
    let c = distance(points[indices[1]], points[indices[2]]);
    let mut sides = [a, b, c];
    sides.sort_by(f64::total_cmp);
    if sides[2] <= f64::EPSILON || sides[0] / sides[2] < 0.08 {
        return None;
    }
    let area_twice = ((points[indices[1]].x - points[indices[0]].x)
        * (points[indices[2]].y - points[indices[0]].y)
        - (points[indices[1]].y - points[indices[0]].y)
            * (points[indices[2]].x - points[indices[0]].x))
        .abs();
    if area_twice / (sides[2] * sides[2]) < 0.015 {
        return None;
    }
    Some(Triangle {
        indices,
        short_ratio: sides[0] / sides[2],
        middle_ratio: sides[1] / sides[2],
    })
}

fn symmetric_triangle_pairs(
    image: &[Triangle],
    catalog: &[Triangle],
    tolerance: f64,
) -> Vec<(Triangle, Triangle)> {
    if image.is_empty() || catalog.is_empty() {
        return Vec::new();
    }
    let nearest_catalog: Vec<_> = image
        .iter()
        .map(|image_triangle| nearest_triangle(*image_triangle, catalog))
        .collect();
    let nearest_image: Vec<_> = catalog
        .iter()
        .map(|catalog_triangle| nearest_triangle(*catalog_triangle, image))
        .collect();
    let maximum_distance_squared = 2.0 * tolerance * tolerance;
    nearest_catalog
        .into_iter()
        .enumerate()
        .filter_map(|(image_index, (catalog_index, distance_squared))| {
            (distance_squared <= maximum_distance_squared
                && nearest_image[catalog_index].0 == image_index)
                .then_some((image[image_index], catalog[catalog_index]))
        })
        .collect()
}

fn nearest_triangle(needle: Triangle, haystack: &[Triangle]) -> (usize, f64) {
    haystack
        .iter()
        .enumerate()
        .map(|(index, candidate)| {
            let dx = needle.short_ratio - candidate.short_ratio;
            let dy = needle.middle_ratio - candidate.middle_ratio;
            (index, dx * dx + dy * dy)
        })
        .min_by(|left, right| left.1.total_cmp(&right.1))
        .unwrap_or((0, f64::INFINITY))
}

fn distance(left: Point, right: Point) -> f64 {
    (left.x - right.x).hypot(left.y - right.y)
}

fn affine_from_three(image: [Point; 3], catalog: [Point; 3]) -> Option<Affine> {
    let design = Matrix3::new(
        image[0].x, image[0].y, 1.0, image[1].x, image[1].y, 1.0, image[2].x, image[2].y, 1.0,
    );
    let x = design
        .lu()
        .solve(&Vector3::new(catalog[0].x, catalog[1].x, catalog[2].x))?;
    let y = design
        .lu()
        .solve(&Vector3::new(catalog[0].y, catalog[1].y, catalog[2].y))?;
    Some(Affine {
        x_x: x[0],
        x_y: x[1],
        x_0: x[2],
        y_x: y[0],
        y_y: y[1],
        y_0: y[2],
    })
}

fn associate(
    affine: Affine,
    image: &[Point],
    catalog: &[Point],
    tolerance_arcsec: f64,
) -> Vec<AstrometricMatch> {
    let mut candidates = Vec::new();
    for image_point in image {
        let projected = affine.apply(*image_point);
        if let Some((catalog_point, residual_deg)) = catalog
            .iter()
            .map(|point| {
                let residual = (projected.x - point.x).hypot(projected.y - point.y);
                (point, residual)
            })
            .min_by(|left, right| left.1.total_cmp(&right.1))
        {
            let residual_arcsec = residual_deg * 3_600.0;
            if residual_arcsec <= tolerance_arcsec {
                candidates.push(AstrometricMatch {
                    image_source_index: image_point.original_index,
                    catalog_source_index: catalog_point.original_index,
                    residual_arcsec,
                    residual_x_arcsec: (projected.x - catalog_point.x) * 3_600.0,
                    residual_y_arcsec: (projected.y - catalog_point.y) * 3_600.0,
                    weight: 1.0,
                    used: true,
                    rejection_reason: None,
                });
            }
        }
    }
    candidates.sort_by(|left, right| left.residual_arcsec.total_cmp(&right.residual_arcsec));
    let mut used_catalog = HashSet::new();
    candidates.retain(|pair| used_catalog.insert(pair.catalog_source_index));
    candidates.sort_by_key(|pair| pair.image_source_index);
    candidates
}

fn fit_affine(
    image: &[Point],
    catalog: &[Point],
    matches: &[AstrometricMatch],
) -> Result<Affine, MatchError> {
    let image_by_original: HashMap<usize, Point> = image
        .iter()
        .map(|point| (point.original_index, *point))
        .collect();
    let catalog_by_original: HashMap<usize, Point> = catalog
        .iter()
        .map(|point| (point.original_index, *point))
        .collect();
    let mut design = DMatrix::zeros(matches.len(), 3);
    let mut target_x = DVector::zeros(matches.len());
    let mut target_y = DVector::zeros(matches.len());
    for (row, pair) in matches.iter().enumerate() {
        let image_point = image_by_original
            .get(&pair.image_source_index)
            .ok_or(MatchError::SingularFit)?;
        let catalog_point = catalog_by_original
            .get(&pair.catalog_source_index)
            .ok_or(MatchError::SingularFit)?;
        design[(row, 0)] = image_point.x;
        design[(row, 1)] = image_point.y;
        design[(row, 2)] = 1.0;
        target_x[row] = catalog_point.x;
        target_y[row] = catalog_point.y;
    }
    let solve = |matrix: &DMatrix<f64>, target: &DVector<f64>| {
        matrix
            .clone()
            .svd(true, true)
            .solve(target, 1.0e-12)
            .map_err(|_| MatchError::SingularFit)
    };
    let mut x = solve(&design, &target_x)?;
    let mut y = solve(&design, &target_y)?;

    // Iteratively reweighted least squares. Measurement weights combine the
    // windowed-centroid uncertainty, propagated Gaia uncertainty and a 30 mas
    // systematic floor; Huber weights prevent one bad reference from steering
    // the plate constants without silently deleting it.
    for _ in 0..6 {
        let scale_deg = (x[0] * y[1] - x[1] * y[0]).abs().sqrt();
        if !scale_deg.is_finite() || scale_deg <= 0.0 {
            return Err(MatchError::SingularFit);
        }
        let residuals: Vec<f64> = (0..matches.len())
            .map(|row| {
                let predicted_x = x[0] * design[(row, 0)] + x[1] * design[(row, 1)] + x[2];
                let predicted_y = y[0] * design[(row, 0)] + y[1] * design[(row, 1)] + y[2];
                (predicted_x - target_x[row]).hypot(predicted_y - target_y[row]) * 3_600.0
            })
            .collect();
        let mut sorted = residuals.clone();
        sorted.sort_by(f64::total_cmp);
        let median = sorted[sorted.len() / 2];
        let mut deviations: Vec<f64> = residuals
            .iter()
            .map(|value| (value - median).abs())
            .collect();
        deviations.sort_by(f64::total_cmp);
        let robust_sigma = (1.4826 * deviations[deviations.len() / 2]).max(0.03);
        let huber_limit = 1.345 * robust_sigma;

        let mut weighted_design = design.clone();
        let mut weighted_x = target_x.clone();
        let mut weighted_y = target_y.clone();
        for (row, pair) in matches.iter().enumerate() {
            let image_point = image_by_original
                .get(&pair.image_source_index)
                .ok_or(MatchError::SingularFit)?;
            let catalog_point = catalog_by_original
                .get(&pair.catalog_source_index)
                .ok_or(MatchError::SingularFit)?;
            let measurement_sigma_deg = (catalog_point.sigma_native.powi(2)
                + (image_point.sigma_native * scale_deg).powi(2)
                + (0.03 / 3_600.0f64).powi(2))
            .sqrt();
            let huber = if residuals[row] <= huber_limit {
                1.0
            } else {
                huber_limit / residuals[row]
            };
            let root_weight = huber.sqrt() / measurement_sigma_deg.max(1.0e-12);
            for column in 0..3 {
                weighted_design[(row, column)] *= root_weight;
            }
            weighted_x[row] *= root_weight;
            weighted_y[row] *= root_weight;
        }
        let next_x = solve(&weighted_design, &weighted_x)?;
        let next_y = solve(&weighted_design, &weighted_y)?;
        let change = (&next_x - &x).norm() + (&next_y - &y).norm();
        x = next_x;
        y = next_y;
        if change < 1.0e-12 {
            break;
        }
    }
    Ok(Affine {
        x_x: x[0],
        x_y: x[1],
        x_0: x[2],
        y_x: y[0],
        y_y: y[1],
        y_0: y[2],
    })
}

fn rms(matches: &[AstrometricMatch]) -> f64 {
    if matches.is_empty() {
        return f64::INFINITY;
    }
    (matches
        .iter()
        .map(|pair| pair.residual_arcsec.powi(2))
        .sum::<f64>()
        / matches.len() as f64)
        .sqrt()
}

const SIP_TREND_THRESHOLD_ARCSEC: f64 = 0.30;
const SIP_MIN_MATCHES_PER_PARAMETER: usize = 3;
/// Alternating CD/polynomial iterations. The physical position tolerance below
/// is the binding stop; this cap only bounds a pathological non-convergent fit.
const SIP_MAX_ALTERNATIONS: usize = 100;
/// Physical convergence: the largest change in the predicted tangent-plane
/// position of any matched star between two alternations must fall below this,
/// in pixels (about 1e-3 arcsec at a 1 arcsec/px plate). The fit is judged
/// converged on this rather than on a parameter-change threshold, because the
/// parameters can keep wiggling in their last digits long after the model has
/// stopped moving physically.
const SIP_POSITION_TOLERANCE_PX: f64 = 1.0e-3;
const SIP_MAX_CORNER_SHIFT_FRACTION: f64 = 0.03;

/// Result of the alternating SIP fit for one polynomial order.
#[derive(Debug, Clone)]
struct SipFit {
    crpix: Vector2<f64>,
    cd: Matrix2<f64>,
    sip: SipDistortion,
    /// False when the alternation hit `SIP_MAX_ALTERNATIONS` without the model
    /// position settling within `SIP_POSITION_TOLERANCE_PX`. A non-converged
    /// fit is never accepted.
    converged: bool,
}

#[derive(Debug, Clone)]
struct SipFitModel {
    crpix1: f64,
    crpix2: f64,
    cd1_1: f64,
    cd1_2: f64,
    cd2_1: f64,
    cd2_2: f64,
    sip: Option<SipDistortion>,
}

impl SipFitModel {
    fn project(&self, x: f64, y: f64) -> Vector2<f64> {
        let u = x - self.crpix1;
        let v = y - self.crpix2;
        let (focal_x, focal_y) = match &self.sip {
            None => (u, v),
            Some(sip) => (u + sip.evaluate_a(u, v), v + sip.evaluate_b(u, v)),
        };
        Vector2::new(
            self.cd1_1 * focal_x + self.cd1_2 * focal_y,
            self.cd2_1 * focal_x + self.cd2_2 * focal_y,
        )
    }

    fn to_wcs(&self, crval1: f64, crval2: f64, width: u32, height: u32) -> Wcs {
        Wcs {
            crpix1: self.crpix1,
            crpix2: self.crpix2,
            crval1,
            crval2,
            cd1_1: self.cd1_1,
            cd1_2: self.cd1_2,
            cd2_1: self.cd2_1,
            cd2_2: self.cd2_2,
            image_width: width,
            image_height: height,
            sip: self.sip.clone(),
        }
    }
}

enum PlateModelOutcome {
    Applied {
        order: usize,
        wcs: Wcs,
        matches: Vec<AstrometricMatch>,
        downgrade_reason: Option<String>,
    },
    LinearWithDowngrade {
        reason: String,
    },
    Rejected {
        reason: String,
        downgrade_reason: Option<String>,
        /// False when no candidate order converged, so a consumer can tell a
        /// genuine distortion rejection from a failed fit.
        fit_converged: bool,
    },
}

fn linear_wcs_from_affine(
    affine: Affine,
    crval1: f64,
    crval2: f64,
    width: u32,
    height: u32,
) -> Result<Wcs, MatchError> {
    let matrix = Matrix2::new(affine.x_x, affine.x_y, affine.y_x, affine.y_y);
    let origin = matrix.try_inverse().ok_or(MatchError::SingularFit)?
        * -Vector2::new(affine.x_0, affine.y_0);
    Ok(Wcs {
        crpix1: origin.x,
        crpix2: origin.y,
        crval1,
        crval2,
        cd1_1: affine.x_x,
        cd1_2: affine.x_y,
        cd2_1: affine.y_x,
        cd2_2: affine.y_y,
        image_width: width,
        image_height: height,
        sip: None,
    })
}

#[allow(clippy::too_many_arguments)]
fn finish_with_plate_model(
    image: &[Point],
    catalog: &[Point],
    affine: Affine,
    matches: Vec<AstrometricMatch>,
    config: &MatchConfig,
    crval1: f64,
    crval2: f64,
    width: u32,
    height: u32,
) -> Result<AstrometricSolution, MatchError> {
    let linear_wcs = linear_wcs_from_affine(affine, crval1, crval2, width, height)?;
    let linear_solution = |wcs: Wcs, matches: Vec<AstrometricMatch>| {
        let rms_arcsec = rms(&matches);
        AstrometricSolution {
            wcs,
            matches,
            rms_arcsec,
            plate_model_requested: config.plate_model,
            plate_model_applied: PlateModel::Linear,
            sip_order: None,
            sip_escalation_attempted: false,
            sip_escalation_succeeded: false,
            sip_fit_converged: None,
            plate_model_downgraded: false,
            plate_model_downgrade_reason: None,
            sip_rejection_reason: None,
        }
    };

    if config.plate_model == PlateModel::Linear {
        return Ok(linear_solution(linear_wcs, matches));
    }

    let trend = spatial_trend_arcsec(&matches, image, width, height);
    if trend <= SIP_TREND_THRESHOLD_ARCSEC {
        return Ok(linear_solution(linear_wcs, matches));
    }

    let outcome = escalate_plate_model(
        image, catalog, affine, &matches, config, crval1, crval2, width, height,
    )?;

    match outcome {
        PlateModelOutcome::LinearWithDowngrade { reason } => {
            let mut solution = linear_solution(linear_wcs, matches);
            solution.plate_model_downgraded = true;
            solution.plate_model_downgrade_reason = Some(reason);
            Ok(solution)
        }
        PlateModelOutcome::Applied {
            order,
            wcs,
            matches,
            downgrade_reason,
        } => {
            let applied = PlateModel::from_order(order);
            let downgraded = downgrade_reason.is_some();
            Ok(AstrometricSolution {
                rms_arcsec: rms(&matches),
                wcs,
                matches,
                plate_model_requested: config.plate_model,
                plate_model_applied: applied,
                sip_order: Some(order),
                sip_escalation_attempted: true,
                sip_escalation_succeeded: true,
                sip_fit_converged: Some(true),
                plate_model_downgraded: downgraded,
                plate_model_downgrade_reason: downgrade_reason,
                sip_rejection_reason: None,
            })
        }
        PlateModelOutcome::Rejected {
            reason,
            downgrade_reason,
            fit_converged,
        } => {
            let mut solution = linear_solution(linear_wcs, matches);
            solution.plate_model_downgraded = downgrade_reason.is_some();
            solution.plate_model_downgrade_reason = downgrade_reason;
            solution.sip_escalation_attempted = true;
            solution.sip_escalation_succeeded = false;
            solution.sip_fit_converged = Some(fit_converged);
            solution.sip_rejection_reason = Some(reason);
            Ok(solution)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn escalate_plate_model(
    image: &[Point],
    catalog: &[Point],
    affine: Affine,
    matches: &[AstrometricMatch],
    config: &MatchConfig,
    crval1: f64,
    crval2: f64,
    width: u32,
    height: u32,
) -> Result<PlateModelOutcome, MatchError> {
    let matrix = affine.matrix();
    let crpix = matrix.try_inverse().ok_or(MatchError::SingularFit)?
        * -Vector2::new(affine.x_0, affine.y_0);
    let scale = f64::from(width.max(height)) * 0.5;
    let requested = config.plate_model.order();

    let supported = highest_supported_order(matches.len(), requested);
    let downgrade_reason = if supported < requested {
        Some(format!(
            "{} 阶板模型需要至少 {} 颗已验证参考星，当前 {} 颗",
            config.plate_model.as_str(),
            sip_minimum_matches(requested),
            matches.len()
        ))
    } else {
        None
    };

    if supported <= 1 {
        return Ok(PlateModelOutcome::LinearWithDowngrade {
            reason: downgrade_reason
                .unwrap_or_else(|| "参考星数量不足以拟合 SIP 畸变模型".to_string()),
        });
    }

    let mut current_matches = matches.to_vec();
    let mut any_converged = false;
    // Warm-start each higher order from the previous order's converged linear
    // part. The alternating scheme converges slowly otherwise, and the CD/CRPIX
    // from a lower order is already close to the higher-order answer.
    let mut seed_crpix = crpix;
    let mut seed_cd = matrix;

    for order in 2..=supported {
        if current_matches.len() < sip_minimum_matches(order) {
            return Ok(PlateModelOutcome::LinearWithDowngrade {
                reason: format!(
                    "{} 阶板模型需要至少 {} 颗已验证参考星，当前 {} 颗",
                    order,
                    sip_minimum_matches(order),
                    current_matches.len()
                ),
            });
        }

        // Refit CD and the tangent-plane offset together, re-anchoring CRPIX on
        // the refined linear part until the alternation converges, so the
        // polynomial ends up in the canonical FITS basis.
        let fit = fit_sip_on_matches(
            image,
            catalog,
            &current_matches,
            seed_crpix,
            seed_cd,
            order,
            scale,
            SIP_MAX_ALTERNATIONS,
        )?;
        seed_crpix = fit.crpix;
        seed_cd = fit.cd;
        let model = SipFitModel {
            crpix1: fit.crpix.x,
            crpix2: fit.crpix.y,
            cd1_1: fit.cd[(0, 0)],
            cd1_2: fit.cd[(0, 1)],
            cd2_1: fit.cd[(1, 0)],
            cd2_2: fit.cd[(1, 1)],
            sip: Some(fit.sip.clone()),
        };

        // Escalation must re-associate one-to-one after every order change.
        let pixel_scale_arcsec = fit.cd.determinant().abs().sqrt() * 3_600.0;
        let wide_tolerance = (pixel_scale_arcsec * (2.5 + 2.0 * order as f64)).clamp(2.0, 60.0);
        let candidates = associate_full(&model, image, catalog, wide_tolerance);
        if candidates.len() < config.minimum_seed_matches {
            current_matches = matches.to_vec();
            continue;
        }
        let candidate_rms = rms(&candidates);
        let clip = (candidate_rms * 2.8).clamp(0.6, wide_tolerance);
        let clipped: Vec<_> = candidates
            .into_iter()
            .filter(|pair| pair.residual_arcsec <= clip)
            .collect();
        if clipped.len() < config.minimum_seed_matches {
            current_matches = matches.to_vec();
            continue;
        }

        // A half-converged model is never accepted: it would silently ship an
        // inconsistent (CRPIX, CD, A/B) decomposition.
        if !fit.converged {
            current_matches = clipped;
            continue;
        }
        any_converged = true;

        let holdout = holdout_validation(
            image,
            catalog,
            &current_matches,
            crpix,
            matrix,
            order,
            scale,
            config,
        )?;
        if !holdout.passed {
            current_matches = clipped;
            continue;
        }

        if !sip_plausible(&fit.sip, fit.crpix.x, fit.crpix.y, width, height) {
            return Ok(PlateModelOutcome::Rejected {
                reason: "implausible_distortion".to_string(),
                downgrade_reason,
                fit_converged: true,
            });
        }

        // Stop at the LOWEST order that resolves the residual trend.
        //
        // Continuing up to the requested order whenever the holdout passes would
        // overfit: `holdout_validation` compares each candidate against the
        // *linear* model, so a higher order inherits the lower order's
        // improvement and passes even when it buys nothing. The trend gate is
        // the physical criterion for "the unmodelled distortion is resolved", so
        // it decides where escalation stops.
        let trend = spatial_trend_arcsec(&clipped, image, width, height);
        if trend <= SIP_TREND_THRESHOLD_ARCSEC {
            return Ok(PlateModelOutcome::Applied {
                order,
                wcs: model.to_wcs(crval1, crval2, width, height),
                matches: clipped,
                downgrade_reason,
            });
        }
        current_matches = clipped;
    }

    Ok(PlateModelOutcome::Rejected {
        reason: if any_converged {
            "distortion_suspected".to_string()
        } else {
            "sip_fit_not_converged".to_string()
        },
        downgrade_reason,
        fit_converged: any_converged,
    })
}

#[allow(clippy::too_many_arguments)]
fn fit_sip_on_matches(
    image: &[Point],
    catalog: &[Point],
    matches: &[AstrometricMatch],
    crpix: Vector2<f64>,
    initial_cd: Matrix2<f64>,
    order: usize,
    scale: f64,
    max_alternations: usize,
) -> Result<SipFit, MatchError> {
    let pairs = resolve_matches(image, catalog, matches)?;
    let mut crpix = crpix;
    let mut cd = initial_cd;
    let mut sip = fit_sip_polynomial(image, catalog, matches, crpix, cd, order, scale)?;
    let mut converged = false;
    for _ in 0..max_alternations {
        let (next_cd, offset) = fit_cd_with_sip(image, catalog, matches, crpix, &sip)?;
        // Re-anchor CRPIX on the refined linear part. The map decomposes
        // uniquely into (CRPIX, CD) plus the p+q>=2 polynomial, so freezing
        // CRPIX while also refitting the offset would make the two halves
        // inconsistent and leave a systematic offset error.
        let inverse = next_cd.try_inverse().ok_or(MatchError::SingularFit)?;
        let next_crpix = crpix - inverse * offset;
        let next_sip =
            fit_sip_polynomial(image, catalog, matches, next_crpix, next_cd, order, scale)?;

        let pixel_scale_deg = next_cd.determinant().abs().sqrt();
        if !pixel_scale_deg.is_finite() || pixel_scale_deg <= 0.0 {
            return Err(MatchError::SingularFit);
        }
        let max_shift_px = pairs
            .iter()
            .map(|(image_point, _)| {
                let before = project_with_model(*image_point, crpix, cd, &sip);
                let after = project_with_model(*image_point, next_crpix, next_cd, &next_sip);
                (after - before).norm() / pixel_scale_deg
            })
            .fold(0.0f64, f64::max);

        crpix = next_crpix;
        cd = next_cd;
        sip = next_sip;
        if max_shift_px < SIP_POSITION_TOLERANCE_PX {
            converged = true;
            break;
        }
    }
    Ok(SipFit {
        crpix,
        cd,
        sip,
        converged,
    })
}

/// Predicted tangent-plane position (degrees) of one image point under a
/// (CRPIX, CD, SIP) model.
fn project_with_model(
    point: Point,
    crpix: Vector2<f64>,
    cd: Matrix2<f64>,
    sip: &SipDistortion,
) -> Vector2<f64> {
    let u = point.x - crpix.x;
    let v = point.y - crpix.y;
    let focal_x = u + sip.evaluate_a(u, v);
    let focal_y = v + sip.evaluate_b(u, v);
    Vector2::new(
        cd[(0, 0)] * focal_x + cd[(0, 1)] * focal_y,
        cd[(1, 0)] * focal_x + cd[(1, 1)] * focal_y,
    )
}

fn fit_sip_polynomial(
    image: &[Point],
    catalog: &[Point],
    matches: &[AstrometricMatch],
    crpix: Vector2<f64>,
    cd: Matrix2<f64>,
    order: usize,
    scale: f64,
) -> Result<SipDistortion, MatchError> {
    let pairs = resolve_matches(image, catalog, matches)?;
    let cd_inverse = cd.try_inverse().ok_or(MatchError::SingularFit)?;
    let scale_deg = cd.determinant().abs().sqrt();
    if !scale_deg.is_finite() || scale_deg <= 0.0 {
        return Err(MatchError::SingularFit);
    }
    let terms = sip_terms(order);
    let mut design = DMatrix::zeros(matches.len(), terms.len());
    let mut target_x = DVector::zeros(matches.len());
    let mut target_y = DVector::zeros(matches.len());
    for (row, (image_point, catalog_point)) in pairs.iter().enumerate() {
        let u = image_point.x - crpix.x;
        let v = image_point.y - crpix.y;
        let focal = cd_inverse * Vector2::new(catalog_point.x, catalog_point.y);
        target_x[row] = focal.x - u;
        target_y[row] = focal.y - v;
        let u_n = u / scale;
        let v_n = v / scale;
        for (column, &(p, q)) in terms.iter().enumerate() {
            design[(row, column)] = u_n.powi(p as i32) * v_n.powi(q as i32);
        }
    }

    let solve = |matrix: &DMatrix<f64>, target: &DVector<f64>| {
        matrix
            .clone()
            .svd(true, true)
            .solve(target, 1.0e-12)
            .map_err(|_| MatchError::SingularFit)
    };
    let mut x = solve(&design, &target_x)?;
    let mut y = solve(&design, &target_y)?;

    // Reuse the affine solver's IRLS weighting: measurement sigma plus a Huber
    // weight. Residuals are projected to the tangent plane so the arcsec scale
    // matches the existing weight model.
    for _ in 0..6 {
        let residuals: Vec<f64> = pairs
            .iter()
            .enumerate()
            .map(|(row, _)| {
                let dx = target_x[row] - predict(&design, &x, row);
                let dy = target_y[row] - predict(&design, &y, row);
                (cd * Vector2::new(dx, dy)).norm() * 3_600.0
            })
            .collect();
        let huber = robust_huber_weights(&residuals);
        let mut weighted_design = design.clone();
        let mut weighted_x = target_x.clone();
        let mut weighted_y = target_y.clone();
        for (row, (image_point, catalog_point)) in pairs.iter().enumerate() {
            let sigma_deg = measurement_sigma_deg(*image_point, *catalog_point, scale_deg);
            let sigma_px = sigma_deg / scale_deg;
            let root_weight = huber[row].sqrt() / sigma_px.max(1.0e-12);
            for column in 0..terms.len() {
                weighted_design[(row, column)] *= root_weight;
            }
            weighted_x[row] *= root_weight;
            weighted_y[row] *= root_weight;
        }
        let next_x = solve(&weighted_design, &weighted_x)?;
        let next_y = solve(&weighted_design, &weighted_y)?;
        let change = (&next_x - &x).norm() + (&next_y - &y).norm();
        x = next_x;
        y = next_y;
        if change < 1.0e-12 {
            break;
        }
    }

    let normalized = SipDistortion::new(order, order, x.as_slice().to_vec(), y.as_slice().to_vec());
    Ok(normalized.to_unnormalized(scale))
}

fn fit_cd_with_sip(
    image: &[Point],
    catalog: &[Point],
    matches: &[AstrometricMatch],
    crpix: Vector2<f64>,
    sip: &SipDistortion,
) -> Result<(Matrix2<f64>, Vector2<f64>), MatchError> {
    let pairs = resolve_matches(image, catalog, matches)?;
    let mut design = DMatrix::zeros(matches.len(), 3);
    let mut target_x = DVector::zeros(matches.len());
    let mut target_y = DVector::zeros(matches.len());
    for (row, (image_point, catalog_point)) in pairs.iter().enumerate() {
        let u = image_point.x - crpix.x;
        let v = image_point.y - crpix.y;
        let focal_x = u + sip.evaluate_a(u, v);
        let focal_y = v + sip.evaluate_b(u, v);
        design[(row, 0)] = focal_x;
        design[(row, 1)] = focal_y;
        design[(row, 2)] = 1.0;
        target_x[row] = catalog_point.x;
        target_y[row] = catalog_point.y;
    }

    let solve = |matrix: &DMatrix<f64>, target: &DVector<f64>| {
        matrix
            .clone()
            .svd(true, true)
            .solve(target, 1.0e-12)
            .map_err(|_| MatchError::SingularFit)
    };
    let mut x = solve(&design, &target_x)?;
    let mut y = solve(&design, &target_y)?;

    for _ in 0..6 {
        let scale_deg = (x[0] * y[1] - x[1] * y[0]).abs().sqrt();
        if !scale_deg.is_finite() || scale_deg <= 0.0 {
            return Err(MatchError::SingularFit);
        }
        let residuals: Vec<f64> = pairs
            .iter()
            .enumerate()
            .map(|(row, _)| {
                let predicted_x = x[0] * design[(row, 0)] + x[1] * design[(row, 1)] + x[2];
                let predicted_y = y[0] * design[(row, 0)] + y[1] * design[(row, 1)] + y[2];
                (predicted_x - target_x[row]).hypot(predicted_y - target_y[row]) * 3_600.0
            })
            .collect();
        let huber = robust_huber_weights(&residuals);
        let mut weighted_design = design.clone();
        let mut weighted_x = target_x.clone();
        let mut weighted_y = target_y.clone();
        for (row, (image_point, catalog_point)) in pairs.iter().enumerate() {
            let sigma_deg = measurement_sigma_deg(*image_point, *catalog_point, scale_deg);
            let root_weight = huber[row].sqrt() / sigma_deg.max(1.0e-12);
            for column in 0..3 {
                weighted_design[(row, column)] *= root_weight;
            }
            weighted_x[row] *= root_weight;
            weighted_y[row] *= root_weight;
        }
        let next_x = solve(&weighted_design, &weighted_x)?;
        let next_y = solve(&weighted_design, &weighted_y)?;
        let change = (&next_x - &x).norm() + (&next_y - &y).norm();
        x = next_x;
        y = next_y;
        if change < 1.0e-12 {
            break;
        }
    }

    Ok((
        Matrix2::new(x[0], x[1], y[0], y[1]),
        Vector2::new(x[2], y[2]),
    ))
}

fn resolve_matches(
    image: &[Point],
    catalog: &[Point],
    matches: &[AstrometricMatch],
) -> Result<Vec<(Point, Point)>, MatchError> {
    let image_by_original: HashMap<usize, Point> = image
        .iter()
        .map(|point| (point.original_index, *point))
        .collect();
    let catalog_by_original: HashMap<usize, Point> = catalog
        .iter()
        .map(|point| (point.original_index, *point))
        .collect();
    matches
        .iter()
        .map(|pair| {
            let image_point = image_by_original
                .get(&pair.image_source_index)
                .ok_or(MatchError::SingularFit)?;
            let catalog_point = catalog_by_original
                .get(&pair.catalog_source_index)
                .ok_or(MatchError::SingularFit)?;
            Ok((*image_point, *catalog_point))
        })
        .collect()
}

fn robust_huber_weights(residuals: &[f64]) -> Vec<f64> {
    if residuals.is_empty() {
        return Vec::new();
    }
    let mut sorted = residuals.to_vec();
    sorted.sort_by(f64::total_cmp);
    let median = sorted[sorted.len() / 2];
    let mut deviations: Vec<f64> = residuals
        .iter()
        .map(|value| (value - median).abs())
        .collect();
    deviations.sort_by(f64::total_cmp);
    let robust_sigma = (1.4826 * deviations[deviations.len() / 2]).max(0.03);
    let huber_limit = 1.345 * robust_sigma;
    residuals
        .iter()
        .map(|value| {
            if *value <= huber_limit {
                1.0
            } else {
                huber_limit / *value
            }
        })
        .collect()
}

fn measurement_sigma_deg(image_point: Point, catalog_point: Point, scale_deg: f64) -> f64 {
    (catalog_point.sigma_native.powi(2)
        + (image_point.sigma_native * scale_deg).powi(2)
        + (0.03 / 3_600.0f64).powi(2))
    .sqrt()
}

fn predict(design: &DMatrix<f64>, coefficients: &DVector<f64>, row: usize) -> f64 {
    (0..design.ncols())
        .map(|column| design[(row, column)] * coefficients[column])
        .sum()
}

fn sip_terms(order: usize) -> Vec<(usize, usize)> {
    let mut terms = Vec::with_capacity(sip_coefficient_count(order));
    for p in 0..=order {
        for q in 0..=order - p {
            if p + q >= 2 {
                terms.push((p, q));
            }
        }
    }
    terms
}

fn sip_coefficient_count(order: usize) -> usize {
    (order + 1) * (order + 2) / 2 - 3
}

fn sip_monomial_count(order: usize) -> usize {
    (order + 1) * (order + 2) / 2
}

fn sip_minimum_matches(order: usize) -> usize {
    2 * sip_monomial_count(order) * SIP_MIN_MATCHES_PER_PARAMETER
}

fn highest_supported_order(matches: usize, requested: usize) -> usize {
    let mut best = 1;
    for order in 2..=requested {
        if matches >= sip_minimum_matches(order) {
            best = order;
        }
    }
    best
}

fn spatial_trend_arcsec(
    matches: &[AstrometricMatch],
    image: &[Point],
    width: u32,
    height: u32,
) -> f64 {
    let image_by_original: HashMap<usize, Point> = image
        .iter()
        .map(|point| (point.original_index, *point))
        .collect();
    let mut center_x = 0.0;
    let mut center_y = 0.0;
    let mut center_count = 0usize;
    let mut edge_x = 0.0;
    let mut edge_y = 0.0;
    let mut edge_count = 0usize;
    for pair in matches {
        if !pair.used || !pair.residual_arcsec.is_finite() {
            continue;
        }
        let Some(point) = image_by_original.get(&pair.image_source_index) else {
            continue;
        };
        let nx = point.x / f64::from(width.max(1)) - 0.5;
        let ny = point.y / f64::from(height.max(1)) - 0.5;
        if nx.abs().max(ny.abs()) >= 0.32 {
            edge_x += pair.residual_x_arcsec;
            edge_y += pair.residual_y_arcsec;
            edge_count += 1;
        } else {
            center_x += pair.residual_x_arcsec;
            center_y += pair.residual_y_arcsec;
            center_count += 1;
        }
    }
    if center_count == 0 || edge_count == 0 {
        return 0.0;
    }
    let center = (
        center_x / center_count as f64,
        center_y / center_count as f64,
    );
    let edge = (edge_x / edge_count as f64, edge_y / edge_count as f64);
    (edge.0 - center.0).hypot(edge.1 - center.1)
}

struct HoldoutResult {
    passed: bool,
}

#[allow(clippy::too_many_arguments)]
fn holdout_validation(
    image: &[Point],
    catalog: &[Point],
    matches: &[AstrometricMatch],
    crpix: Vector2<f64>,
    linear_cd: Matrix2<f64>,
    order: usize,
    scale: f64,
    config: &MatchConfig,
) -> Result<HoldoutResult, MatchError> {
    if matches.len() < 8 {
        return Ok(HoldoutResult { passed: false });
    }
    let holdout_indices =
        radial_stratified_holdout(image, matches, crpix, config.sip_holdout_fraction);
    if holdout_indices.is_empty() {
        return Ok(HoldoutResult { passed: false });
    }
    let holdout_set: HashSet<usize> = holdout_indices.iter().copied().collect();
    let train: Vec<AstrometricMatch> = matches
        .iter()
        .enumerate()
        .filter(|(index, _)| !holdout_set.contains(index))
        .map(|(_, pair)| pair.clone())
        .collect();
    if train.len() < sip_minimum_matches(order) {
        return Ok(HoldoutResult { passed: false });
    }

    let fit = fit_sip_on_matches(
        image,
        catalog,
        &train,
        crpix,
        linear_cd,
        order,
        scale,
        SIP_MAX_ALTERNATIONS,
    )?;
    if !fit.converged {
        return Ok(HoldoutResult { passed: false });
    }
    let model = SipFitModel {
        crpix1: fit.crpix.x,
        crpix2: fit.crpix.y,
        cd1_1: fit.cd[(0, 0)],
        cd1_2: fit.cd[(0, 1)],
        cd2_1: fit.cd[(1, 0)],
        cd2_2: fit.cd[(1, 1)],
        sip: Some(fit.sip),
    };

    let image_by_original: HashMap<usize, Point> = image
        .iter()
        .map(|point| (point.original_index, *point))
        .collect();
    let catalog_by_original: HashMap<usize, Point> = catalog
        .iter()
        .map(|point| (point.original_index, *point))
        .collect();
    let mut candidate_sum = 0.0;
    let mut linear_sum = 0.0;
    let mut count = 0usize;
    for index in &holdout_indices {
        let pair = &matches[*index];
        let Some(image_point) = image_by_original.get(&pair.image_source_index) else {
            continue;
        };
        let Some(catalog_point) = catalog_by_original.get(&pair.catalog_source_index) else {
            continue;
        };
        let candidate_projected = model.project(image_point.x, image_point.y);
        let candidate_residual_deg = (candidate_projected.x - catalog_point.x)
            .hypot(candidate_projected.y - catalog_point.y);
        candidate_sum += (candidate_residual_deg * 3_600.0).powi(2);
        let linear_projected =
            linear_cd * Vector2::new(image_point.x - crpix.x, image_point.y - crpix.y);
        let linear_residual_deg =
            (linear_projected.x - catalog_point.x).hypot(linear_projected.y - catalog_point.y);
        linear_sum += (linear_residual_deg * 3_600.0).powi(2);
        count += 1;
    }
    if count == 0 {
        return Ok(HoldoutResult { passed: false });
    }
    let candidate_rms = (candidate_sum / count as f64).sqrt();
    let linear_rms = (linear_sum / count as f64).sqrt();
    let improved = candidate_rms <= linear_rms * (1.0 - config.sip_holdout_improvement_fraction);
    let within_limit = candidate_rms <= config.maximum_rms_arcsec;
    Ok(HoldoutResult {
        passed: improved && within_limit,
    })
}

fn radial_stratified_holdout(
    image: &[Point],
    matches: &[AstrometricMatch],
    crpix: Vector2<f64>,
    fraction: f64,
) -> Vec<usize> {
    let image_by_original: HashMap<usize, Point> = image
        .iter()
        .map(|point| (point.original_index, *point))
        .collect();
    let mut indexed: Vec<(usize, f64)> = matches
        .iter()
        .enumerate()
        .filter_map(|(index, pair)| {
            image_by_original
                .get(&pair.image_source_index)
                .map(|point| (index, (point.x - crpix.x).hypot(point.y - crpix.y)))
        })
        .collect();
    indexed.sort_by(|left, right| left.1.total_cmp(&right.1));
    let strata = 5usize;
    let per_stratum = (indexed.len() as f64 / strata as f64).ceil() as usize;
    let mut holdout = Vec::new();
    for chunk in indexed.chunks(per_stratum.max(1)) {
        if chunk.is_empty() {
            continue;
        }
        let take = ((chunk.len() as f64 * fraction).round() as usize).max(1);
        for offset in 0..take {
            let position = if take <= 1 {
                0
            } else {
                offset * (chunk.len() - 1) / (take - 1)
            };
            holdout.push(chunk[position].0);
        }
    }
    holdout
}

fn sip_plausible(sip: &SipDistortion, crpix1: f64, crpix2: f64, width: u32, height: u32) -> bool {
    let frame_extent = f64::from(width).hypot(f64::from(height));
    if frame_extent <= 0.0 {
        return false;
    }

    // Corner shift guard: the polynomial may not push a frame corner by more
    // than a few percent of the frame extent.
    for (x, y) in [
        (0.0, 0.0),
        (f64::from(width), 0.0),
        (0.0, f64::from(height)),
        (f64::from(width), f64::from(height)),
    ] {
        let u = x - crpix1;
        let v = y - crpix2;
        let shift = sip.evaluate_a(u, v).hypot(sip.evaluate_b(u, v));
        if shift > SIP_MAX_CORNER_SHIFT_FRACTION * frame_extent {
            return false;
        }
    }

    // Radial monotonicity guard: the correction magnitude should grow outward
    // and must not have a significant interior extremum (an overfit wiggle).
    let scale = f64::from(width.max(height)) * 0.5;
    let step = ((width.max(height) as usize) / 16).max(1);
    let bands = [0.25, 0.5, 0.75, 1.0];
    let mut band_sums = [0.0f64; 4];
    let mut band_counts = [0usize; 4];
    let mut x = 0usize;
    while x <= width as usize {
        let mut y = 0usize;
        while y <= height as usize {
            let u = x as f64 - crpix1;
            let v = y as f64 - crpix2;
            let radius = u.hypot(v) / scale;
            if radius <= 1.0 {
                let correction = sip.evaluate_a(u, v).hypot(sip.evaluate_b(u, v));
                let band = if radius < bands[0] {
                    0
                } else if radius < bands[1] {
                    1
                } else if radius < bands[2] {
                    2
                } else {
                    3
                };
                band_sums[band] += correction;
                band_counts[band] += 1;
            }
            y += step;
        }
        x += step;
    }
    let mut previous_mean: Option<f64> = None;
    for band in 0..4 {
        if band_counts[band] == 0 {
            continue;
        }
        let mean = band_sums[band] / band_counts[band] as f64;
        if let Some(previous) = previous_mean {
            if previous > mean * 1.25 + 1.0e-6 {
                return false;
            }
        }
        previous_mean = Some(mean);
    }
    true
}

fn associate_full(
    model: &SipFitModel,
    image: &[Point],
    catalog: &[Point],
    tolerance_arcsec: f64,
) -> Vec<AstrometricMatch> {
    let mut candidates = Vec::new();
    for image_point in image {
        let projected = model.project(image_point.x, image_point.y);
        if let Some((catalog_point, residual_deg)) = catalog
            .iter()
            .map(|point| {
                let residual = (projected.x - point.x).hypot(projected.y - point.y);
                (point, residual)
            })
            .min_by(|left, right| left.1.total_cmp(&right.1))
        {
            let residual_arcsec = residual_deg * 3_600.0;
            if residual_arcsec <= tolerance_arcsec {
                candidates.push(AstrometricMatch {
                    image_source_index: image_point.original_index,
                    catalog_source_index: catalog_point.original_index,
                    residual_arcsec,
                    residual_x_arcsec: (projected.x - catalog_point.x) * 3_600.0,
                    residual_y_arcsec: (projected.y - catalog_point.y) * 3_600.0,
                    weight: 1.0,
                    used: true,
                    rejection_reason: None,
                });
            }
        }
    }
    candidates.sort_by(|left, right| left.residual_arcsec.total_cmp(&right.residual_arcsec));
    let mut used_catalog = HashSet::new();
    candidates.retain(|pair| used_catalog.insert(pair.catalog_source_index));
    candidates.sort_by_key(|pair| pair.image_source_index);
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_source(x: f64, y: f64, index: usize) -> SourceMeasurement {
        SourceMeasurement {
            x,
            y,
            peak: 1_000.0 - index as f32,
            flux: 100_000.0 - index as f64,
            fwhm: 3.0,
            ellipticity: 0.1,
            npix: 20,
            flags: 0,
            saturated: false,
            snr: Some(100.0),
            x_error_px: Some(0.03),
            y_error_px: Some(0.03),
            centroid_refined: true,
        }
    }

    fn make_gaia(index: usize, ra: f64, dec: f64) -> GaiaSource {
        GaiaSource {
            source_id: index.to_string(),
            ra_deg: ra,
            dec_deg: dec,
            catalog_ra_deg: ra,
            catalog_dec_deg: dec,
            pm_ra_mas_per_year: None,
            pm_dec_mas_per_year: None,
            ra_error_mas: Some(0.2),
            dec_error_mas: Some(0.2),
            pm_ra_error_mas_per_year: Some(0.1),
            pm_dec_error_mas_per_year: Some(0.1),
            ra_dec_correlation: Some(0.0),
            parallax_mas: None,
            parallax_error_mas: None,
            ruwe: Some(1.0),
            duplicated_source: false,
            astrometric_params_solved: Some(31),
            propagated_ra_error_mas: Some(0.3),
            propagated_dec_error_mas: Some(0.3),
            g_mag: Some(10.0 + index as f32 * 0.01),
            epoch_year: 2016.0,
        }
    }

    struct SipField {
        truth: Wcs,
        image: Vec<SourceMeasurement>,
        catalog: Vec<GaiaSource>,
    }

    /// Deterministic low-discrepancy star positions. A regular grid is
    /// degenerate for triangle/pair matching because transposed and rotated
    /// associations fit equally well, so the distortion tests need an
    /// aperiodic spread.
    fn spread_positions(width: u32, height: u32, count: usize) -> Vec<(f64, f64)> {
        let margin = 60.0;
        let span_x = f64::from(width) - 2.0 * margin;
        let span_y = f64::from(height) - 2.0 * margin;
        (0..count)
            .map(|index| {
                let fx = (index as f64 * 0.754_877_666_246_689_6).fract();
                let fy = (index as f64 * 0.569_840_286_295_432_1 + 0.31).fract();
                (margin + span_x * fx, margin + span_y * fy)
            })
            .collect()
    }

    /// Builds a noiseless field by forward-projecting pixel positions through
    /// the truth model (CD + optional SIP).
    fn sip_field(
        width: u32,
        height: u32,
        positions: &[(f64, f64)],
        pixel_scale_arcsec: f64,
        sip: Option<SipDistortion>,
    ) -> SipField {
        let truth = Wcs {
            crpix1: f64::from(width) * 0.5,
            crpix2: f64::from(height) * 0.5,
            crval1: 120.0,
            crval2: 22.0,
            cd1_1: -pixel_scale_arcsec / 3_600.0,
            cd1_2: 0.19 * pixel_scale_arcsec / 3_600.0,
            cd2_1: 0.16 * pixel_scale_arcsec / 3_600.0,
            cd2_2: 0.97 * pixel_scale_arcsec / 3_600.0,
            image_width: width,
            image_height: height,
            sip,
        };
        let mut image = Vec::new();
        let mut catalog = Vec::new();
        for (index, &(x, y)) in positions.iter().enumerate() {
            let (ra, dec) = truth.pixel_to_sky(x, y);
            image.push(make_source(x, y, index));
            catalog.push(make_gaia(index, ra, dec));
        }
        SipField {
            truth,
            image,
            catalog,
        }
    }

    fn spread_field(count: usize, sip: Option<SipDistortion>) -> SipField {
        sip_field(1024, 1024, &spread_positions(1024, 1024, count), 1.15, sip)
    }

    fn spread_field_at_scale(
        count: usize,
        pixel_scale_arcsec: f64,
        sip: Option<SipDistortion>,
    ) -> SipField {
        sip_field(
            1024,
            1024,
            &spread_positions(1024, 1024, count),
            pixel_scale_arcsec,
            sip,
        )
    }

    /// Quadratic distortion A = amp*u_n^2, B = amp*v_n^2. The u^2/v^2 terms
    /// have a positive mean over the frame, which is exactly what produces the
    /// edge-versus-centre spatial residual trend the escalation keys on.
    fn quadratic_sip(scale: f64, amp: f64) -> SipDistortion {
        let normalizer = scale.powi(2);
        SipDistortion::new(
            2,
            2,
            vec![0.0, 0.0, amp / normalizer],
            vec![amp / normalizer, 0.0, 0.0],
        )
    }

    /// Cubic distortion A = amp*u_n^3, B = amp*v_n^3 in the unnormalized basis.
    fn cubic_sip(scale: f64, amp: f64) -> SipDistortion {
        let normalizer = scale.powi(3);
        SipDistortion::new(
            3,
            3,
            vec![0.0, 0.0, 0.0, 0.0, 0.0, 0.0, amp / normalizer],
            vec![0.0, amp / normalizer, 0.0, 0.0, 0.0, 0.0, 0.0],
        )
    }

    fn solve_flagged(field: &SipField, config: MatchConfig) -> AstrometricSolution {
        solve_near_field(
            &field.image,
            &field.catalog,
            field.truth.crval1,
            field.truth.crval2,
            field.truth.image_width,
            field.truth.image_height,
            config,
        )
        .expect("synthetic field should solve")
    }

    /// Config for the synthetic distortion tests: a scale hint keeps pair
    /// voting fast and deterministic, and the source caps are lifted so the
    /// higher-order models have enough references.
    fn distorted_config(plate_model: PlateModel) -> MatchConfig {
        distorted_config_at_scale(plate_model, 1.15)
    }

    fn distorted_config_at_scale(plate_model: PlateModel, pixel_scale_arcsec: f64) -> MatchConfig {
        MatchConfig {
            plate_model,
            max_image_sources: 200,
            max_catalog_sources: 300,
            pixel_scale_hint_arcsec: Some(pixel_scale_arcsec),
            max_candidate_evaluations: 0,
            ..MatchConfig::default()
        }
    }

    #[test]
    fn solves_rotated_reflected_field_with_outliers() {
        let truth = Wcs {
            crpix1: 512.0,
            crpix2: 384.0,
            crval1: 120.0,
            crval2: 22.0,
            cd1_1: -1.15 / 3_600.0,
            cd1_2: 0.22 / 3_600.0,
            cd2_1: 0.18 / 3_600.0,
            cd2_2: 1.12 / 3_600.0,
            image_width: 1024,
            image_height: 768,
            sip: None,
        };
        let mut image = Vec::new();
        let mut catalog = Vec::new();
        for index in 0..24 {
            let x = 70.0 + ((index * 193) % 850) as f64;
            let y = 55.0 + ((index * 137 + index * index * 11) % 650) as f64;
            let noise_x = ((index % 5) as f64 - 2.0) * 0.03;
            let noise_y = ((index % 7) as f64 - 3.0) * 0.025;
            image.push(SourceMeasurement {
                x: x + noise_x,
                y: y + noise_y,
                peak: 1_000.0 - index as f32,
                flux: 100_000.0 - index as f64 * 1_000.0,
                fwhm: 3.0,
                ellipticity: 0.1,
                npix: 20,
                flags: 0,
                saturated: false,
                snr: Some(100.0),
                x_error_px: Some(0.03),
                y_error_px: Some(0.03),
                centroid_refined: true,
            });
            let (ra, dec) = truth.pixel_to_sky(x, y);
            catalog.push(GaiaSource {
                source_id: index.to_string(),
                ra_deg: ra,
                dec_deg: dec,
                catalog_ra_deg: ra,
                catalog_dec_deg: dec,
                pm_ra_mas_per_year: None,
                pm_dec_mas_per_year: None,
                ra_error_mas: Some(0.2),
                dec_error_mas: Some(0.2),
                pm_ra_error_mas_per_year: Some(0.1),
                pm_dec_error_mas_per_year: Some(0.1),
                ra_dec_correlation: Some(0.0),
                parallax_mas: None,
                parallax_error_mas: None,
                ruwe: Some(1.0),
                duplicated_source: false,
                astrometric_params_solved: Some(31),
                propagated_ra_error_mas: Some(0.3),
                propagated_dec_error_mas: Some(0.3),
                g_mag: Some(10.0 + index as f32 * 0.1),
                epoch_year: 2016.0,
            });
        }
        for index in 0..5 {
            image.push(SourceMeasurement {
                x: 100.0 + index as f64 * 83.0,
                y: 700.0 - index as f64 * 71.0,
                peak: 400.0,
                flux: 40_000.0 - index as f64,
                fwhm: 2.5,
                ellipticity: 0.2,
                npix: 12,
                flags: 0,
                saturated: false,
                snr: Some(10.0),
                x_error_px: Some(0.2),
                y_error_px: Some(0.2),
                centroid_refined: true,
            });
        }

        let magnitude_band = quality_catalog_points(
            &catalog,
            120.0,
            22.0,
            &MatchConfig {
                catalog_bright_limit_mag: Some(10.5),
                catalog_faint_limit_mag: Some(11.0),
                ..MatchConfig::default()
            },
        );
        assert_eq!(magnitude_band.len(), 6);
        assert!(magnitude_band
            .iter()
            .all(|point| (5..=10).contains(&point.original_index)));

        let solution = solve_near_field(
            &image,
            &catalog,
            120.0,
            22.0,
            1024,
            768,
            MatchConfig::default(),
        )
        .unwrap();
        assert!(solution.matches.len() >= 20);
        assert!(solution.rms_arcsec < 0.2);
        let (ra, dec) = solution.wcs.pixel_to_sky(800.0, 600.0);
        let (truth_ra, truth_dec) = truth.pixel_to_sky(800.0, 600.0);
        assert!((ra - truth_ra).abs() * 3_600.0 < 0.2);
        assert!((dec - truth_dec).abs() * 3_600.0 < 0.2);

        let mut manual_seed = truth;
        manual_seed.crpix1 += 8.0;
        manual_seed.crpix2 -= 6.0;
        let manual_solution =
            refine_from_wcs_seed(&image, &catalog, manual_seed, MatchConfig::default()).unwrap();
        assert!(manual_solution.matches.len() >= 20);
        assert!(manual_solution.rms_arcsec < 0.2);

        let hinted_solution = solve_near_field(
            &image,
            &catalog,
            120.0,
            22.0,
            1024,
            768,
            MatchConfig {
                pixel_scale_hint_arcsec: Some(1.15),
                parity_hint: Some(true),
                max_candidate_evaluations: 0,
                ..MatchConfig::default()
            },
        )
        .unwrap();
        assert!(hinted_solution.matches.len() >= 20);
        assert!(hinted_solution.rms_arcsec < 0.2);
    }

    #[test]
    fn linear_plate_model_never_produces_sip() {
        // A strongly distorted field, but the shipped default must still return
        // exactly today's 6-parameter affine + TAN/CD solution.
        let field = spread_field(90, Some(quadratic_sip(512.0, 1.0)));
        let solution = solve_flagged(&field, distorted_config(PlateModel::Linear));
        assert_eq!(solution.plate_model_requested, PlateModel::Linear);
        assert_eq!(solution.plate_model_applied, PlateModel::Linear);
        assert!(solution.wcs.sip.is_none());
        assert!(solution.sip_order.is_none());
        assert!(!solution.sip_escalation_attempted);
        assert!(solution.sip_fit_converged.is_none());
    }

    #[test]
    fn quadratic_plate_model_recovers_injected_distortion() {
        let injected = quadratic_sip(512.0, 1.0);
        let field = spread_field(90, Some(injected.clone()));
        let solution = solve_flagged(&field, distorted_config(PlateModel::Quadratic));
        assert_eq!(solution.plate_model_applied, PlateModel::Quadratic);
        assert_eq!(solution.sip_order, Some(2));
        assert!(solution.sip_escalation_succeeded);
        assert_eq!(solution.sip_fit_converged, Some(true));
        assert!((solution.wcs.crpix1 - field.truth.crpix1).abs() < 3.0e-3);
        assert!((solution.wcs.crpix2 - field.truth.crpix2).abs() < 3.0e-3);
        assert!(solution.rms_arcsec < 0.01);
        let recovered = solution.wcs.sip.as_ref().expect("quadratic SIP applied");
        assert_sip_effect_close(
            recovered,
            (solution.wcs.crpix1, solution.wcs.crpix2),
            &injected,
            (field.truth.crpix1, field.truth.crpix2),
            480.0,
            0.02,
        );
        assert_pixel_to_sky_matches_truth(&solution.wcs, &field.truth, 1024, 1024, 58.0, 0.05);
    }

    #[test]
    fn cubic_request_stops_at_the_lowest_order_that_resolves_the_trend() {
        // A cubic request on a purely quadratic field must not escalate past
        // quadratic. `holdout_validation` compares every candidate order against
        // the *linear* model, so order 3 also passes on a quadratic field; the
        // trend gate is what keeps the accepted order minimal and avoids eight
        // unjustified extra parameters plus polynomial extrapolation error
        // outside the star region.
        let injected = quadratic_sip(512.0, 1.0);
        let field = spread_field(90, Some(injected));
        let solution = solve_flagged(&field, distorted_config(PlateModel::Cubic));
        assert_eq!(solution.plate_model_applied, PlateModel::Quadratic);
        assert_eq!(solution.sip_order, Some(2));
        assert!(solution.sip_escalation_succeeded);
        assert!(solution.rms_arcsec < 0.01);
    }

    #[test]
    fn cubic_plate_model_recovers_injected_distortion() {
        // A 3 arcsec/px plate so the third-order term reaches the escalation
        // trend threshold at a distortion comparable to the quadratic fixture.
        let injected = cubic_sip(512.0, 6.0);
        let field = spread_field_at_scale(90, 3.0, Some(injected.clone()));
        let solution = solve_flagged(&field, distorted_config_at_scale(PlateModel::Cubic, 3.0));
        assert_eq!(solution.plate_model_applied, PlateModel::Cubic);
        assert_eq!(solution.sip_order, Some(3));
        assert!(solution.sip_escalation_succeeded);
        assert_eq!(solution.sip_fit_converged, Some(true));
        assert!(solution.rms_arcsec < 0.05);
        let recovered = solution.wcs.sip.as_ref().expect("cubic SIP applied");
        assert_sip_effect_close(
            recovered,
            (solution.wcs.crpix1, solution.wcs.crpix2),
            &injected,
            (field.truth.crpix1, field.truth.crpix2),
            480.0,
            0.02,
        );
        assert_pixel_to_sky_matches_truth(&solution.wcs, &field.truth, 1024, 1024, 58.0, 0.05);
    }

    #[test]
    fn quadratic_plate_model_fabricates_no_distortion_on_undistorted_field() {
        let field = spread_field(90, None);
        let solution = solve_flagged(&field, distorted_config(PlateModel::Quadratic));
        // With no residual trend there is nothing to fit: the escalation must
        // not run and no SIP polynomial may be fabricated.
        assert!(!solution.sip_escalation_attempted);
        assert!(solution.sip_fit_converged.is_none());
        assert_eq!(solution.plate_model_applied, PlateModel::Linear);
        assert!(solution.wcs.sip.is_none());
        assert_eq!(solution.plate_model_requested, PlateModel::Quadratic);
    }

    #[test]
    fn cubic_request_downgrades_when_reference_count_is_insufficient() {
        // 20 validated references cannot support cubic (needs 60) or even
        // quadratic (needs 36), so the request must be reported as downgraded
        // rather than silently returning a linear solution.
        let field = spread_field(20, Some(quadratic_sip(512.0, 1.5)));
        let solution = solve_flagged(&field, distorted_config(PlateModel::Cubic));
        assert_eq!(solution.plate_model_requested, PlateModel::Cubic);
        assert_eq!(solution.plate_model_applied, PlateModel::Linear);
        assert!(solution.plate_model_downgraded);
        assert!(solution.sip_order.is_none());
        assert!(solution.wcs.sip.is_none());
        let reason = solution
            .plate_model_downgrade_reason
            .as_deref()
            .expect("downgrade must be reported");
        assert!(reason.contains("cubic"), "reason: {reason}");
    }

    #[test]
    fn sip_fit_reports_non_convergence_when_the_iteration_cap_is_hit() {
        let field = spread_field(90, Some(quadratic_sip(512.0, 1.0)));
        let linear = solve_flagged(&field, distorted_config(PlateModel::Linear));
        let points = quality_image_points(
            &field.image,
            1024,
            1024,
            &distorted_config(PlateModel::Linear),
        );
        let catalog_points = quality_catalog_points(
            &field.catalog,
            field.truth.crval1,
            field.truth.crval2,
            &distorted_config(PlateModel::Linear),
        );
        let crpix = Vector2::new(linear.wcs.crpix1, linear.wcs.crpix2);
        let cd = Matrix2::new(
            linear.wcs.cd1_1,
            linear.wcs.cd1_2,
            linear.wcs.cd2_1,
            linear.wcs.cd2_2,
        );
        // One alternation is never enough for a distorted field, so the cap is
        // hit without the position criterion being met.
        let capped = fit_sip_on_matches(
            &points,
            &catalog_points,
            &linear.matches,
            crpix,
            cd,
            2,
            512.0,
            1,
        )
        .expect("fit should still return its last state");
        assert!(!capped.converged);
        let settled = fit_sip_on_matches(
            &points,
            &catalog_points,
            &linear.matches,
            crpix,
            cd,
            2,
            512.0,
            SIP_MAX_ALTERNATIONS,
        )
        .expect("fit should converge");
        assert!(settled.converged);
    }

    #[test]
    fn sip_plausibility_guard_rejects_a_corner_shift_larger_than_three_percent() {
        let crpix = Vector2::new(512.0, 512.0);
        // A 1 px quadratic distortion over a 1024x1024 frame is plausible...
        assert!(sip_plausible(
            &quadratic_sip(512.0, 1.0),
            crpix.x,
            crpix.y,
            1024,
            1024
        ));
        // ... but a polynomial that pushes the frame corners by ~140 px is not.
        let huge = SipDistortion::new(
            2,
            2,
            vec![0.0, 0.0, 100.0 / 512.0f64.powi(2)],
            vec![100.0 / 512.0f64.powi(2), 0.0, 0.0],
        );
        assert!(!sip_plausible(&huge, crpix.x, crpix.y, 1024, 1024));
    }

    /// Compares the distortion correction the recovered polynomial applies at
    /// the same physical pixels as the injected one. Comparing raw coefficients
    /// is not meaningful because the two fits use slightly different (converged)
    /// reference pixels, so a small cross-order coefficient can look far off
    /// while its effect on the map is negligible. The correction in pixels is
    /// basis-independent.
    fn assert_sip_effect_close(
        recovered: &SipDistortion,
        recovered_crpix: (f64, f64),
        injected: &SipDistortion,
        injected_crpix: (f64, f64),
        half_extent: f64,
        tolerance_fraction: f64,
    ) {
        assert_eq!(recovered.a_order, injected.a_order);
        assert_eq!(recovered.b_order, injected.b_order);
        let mut max_injected = 0.0f64;
        let mut max_error = 0.0f64;
        for step_x in 0..=8 {
            for step_y in 0..=8 {
                let u_injected = -half_extent + 2.0 * half_extent * f64::from(step_x) / 8.0;
                let v_injected = -half_extent + 2.0 * half_extent * f64::from(step_y) / 8.0;
                let injected_vector = (
                    injected.evaluate_a(u_injected, v_injected),
                    injected.evaluate_b(u_injected, v_injected),
                );
                // The same physical pixel, relative to the recovered CRPIX.
                let u_recovered = u_injected + injected_crpix.0 - recovered_crpix.0;
                let v_recovered = v_injected + injected_crpix.1 - recovered_crpix.1;
                let recovered_vector = (
                    recovered.evaluate_a(u_recovered, v_recovered),
                    recovered.evaluate_b(u_recovered, v_recovered),
                );
                max_injected = max_injected.max(injected_vector.0.hypot(injected_vector.1));
                max_error = max_error.max(
                    (injected_vector.0 - recovered_vector.0)
                        .hypot(injected_vector.1 - recovered_vector.1),
                );
            }
        }
        assert!(
            max_injected > 0.0,
            "injected distortion must be non-trivial"
        );
        assert!(
            max_error <= tolerance_fraction * max_injected,
            "distortion error {max_error:e} px exceeds {:.1}% of injected {max_injected:e} px",
            tolerance_fraction * 100.0
        );
    }

    /// Checks the recovered WCS against the truth over the region the stars
    /// actually sample, and separately checks that the WCS is invertible across
    /// the whole frame. Probing the map outside the sampled region would only
    /// measure polynomial extrapolation, not the fit.
    fn assert_pixel_to_sky_matches_truth(
        recovered: &Wcs,
        truth: &Wcs,
        width: u32,
        height: u32,
        margin: f64,
        tolerance_arcsec: f64,
    ) {
        let span_x = f64::from(width) - 2.0 * margin;
        let span_y = f64::from(height) - 2.0 * margin;
        for step_x in 0..=4 {
            for step_y in 0..=4 {
                let x = margin + span_x * f64::from(step_x) / 4.0;
                let y = margin + span_y * f64::from(step_y) / 4.0;
                let (got_ra, got_dec) = recovered.pixel_to_sky(x, y);
                let (want_ra, want_dec) = truth.pixel_to_sky(x, y);
                let delta_ra = (got_ra - want_ra).abs() * 3_600.0 * want_dec.to_radians().cos();
                let delta_dec = (got_dec - want_dec).abs() * 3_600.0;
                assert!(
                    delta_ra.hypot(delta_dec) <= tolerance_arcsec,
                    "pixel ({x},{y}) offset {:.4} arcsec",
                    delta_ra.hypot(delta_dec)
                );
            }
        }
        for (x, y) in [
            (0.0, 0.0),
            (f64::from(width), 0.0),
            (0.0, f64::from(height)),
            (f64::from(width), f64::from(height)),
            (f64::from(width) * 0.5, f64::from(height) * 0.5),
        ] {
            let (ra, dec) = recovered.pixel_to_sky(x, y);
            let (roundtrip_x, roundtrip_y) = recovered.sky_to_pixel(ra, dec);
            assert!((roundtrip_x - x).abs() < 0.01, "round-trip x at ({x},{y})");
            assert!((roundtrip_y - y).abs() < 0.01, "round-trip y at ({x},{y})");
        }
    }
}
