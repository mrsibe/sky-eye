use crate::{astrometry::wcs::Wcs, catalog::vizier::GaiaSource, reduction::SourceMeasurement};
use delaunator::{triangulate, Point as DelaunayPoint};
use nalgebra::{DMatrix, DVector, Matrix2, Matrix3, Vector2, Vector3};
use serde::Serialize;
use std::collections::{HashMap, HashSet};

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
    let rms_arcsec = rms(&matches);
    if rms_arcsec > config.maximum_rms_arcsec {
        return Err(MatchError::ExcessiveResidual {
            rms_arcsec,
            limit_arcsec: config.maximum_rms_arcsec,
        });
    }

    let matrix = Matrix2::new(affine.x_x, affine.x_y, affine.y_x, affine.y_y);
    let origin = matrix.try_inverse().ok_or(MatchError::SingularFit)?
        * -Vector2::new(affine.x_0, affine.y_0);
    let wcs = Wcs {
        crpix1: origin.x,
        crpix2: origin.y,
        crval1: center_ra_deg,
        crval2: center_dec_deg,
        cd1_1: affine.x_x,
        cd1_2: affine.x_y,
        cd2_1: affine.y_x,
        cd2_2: affine.y_y,
        image_width,
        image_height,
        sip: None,
    };
    Ok(AstrometricSolution {
        wcs,
        matches,
        rms_arcsec,
    })
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
    let rms_arcsec = rms(&matches);
    if rms_arcsec > config.maximum_rms_arcsec {
        return Err(MatchError::ExcessiveResidual {
            rms_arcsec,
            limit_arcsec: config.maximum_rms_arcsec,
        });
    }
    let origin = affine
        .matrix()
        .try_inverse()
        .ok_or(MatchError::SingularFit)?
        * -Vector2::new(affine.x_0, affine.y_0);
    Ok(AstrometricSolution {
        wcs: Wcs {
            crpix1: origin.x,
            crpix2: origin.y,
            crval1: seed.crval1,
            crval2: seed.crval2,
            cd1_1: affine.x_x,
            cd1_2: affine.x_y,
            cd2_1: affine.y_x,
            cd2_2: affine.y_y,
            image_width: seed.image_width,
            image_height: seed.image_height,
            sip: None,
        },
        matches,
        rms_arcsec,
    })
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
