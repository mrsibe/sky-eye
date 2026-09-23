use serde::{Deserialize, Serialize};

/// Simple Imaging Polynomial (SIP) distortion coefficients.
///
/// The forward model follows the FITS SIP convention (Shupe et al. 2005),
/// matching WCSLIB and Astropy. Coefficients are in pixel units and are
/// applied to the pixel offsets BEFORE the CD matrix:
///
///   u = x - crpix1,  v = y - crpix2
///   focal_x = u + sum_{p+q>=2} A_{p,q} u^p v^q
///   focal_y = v + sum_{p+q>=2} B_{p,q} u^p v^q
///   xi  = cd1_1*focal_x + cd1_2*focal_y
///   eta = cd2_1*focal_x + cd2_2*focal_y
///
/// Constant and linear terms (p+q <= 1) are excluded, matching WCSLIB
/// behaviour. Coefficients are stored in the FITS (unnormalized pixel) basis
/// and flattened in row-major order over `p`, with `q` ascending within each
/// row (`p = 0..=order`, `q = 0..=order-p`, keeping `p+q >= 2`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SipDistortion {
    pub a_order: usize,
    pub b_order: usize,
    pub a: Vec<f64>,
    pub b: Vec<f64>,
}

impl SipDistortion {
    pub fn new(a_order: usize, b_order: usize, a: Vec<f64>, b: Vec<f64>) -> Self {
        debug_assert_eq!(a.len(), Self::coefficient_count(a_order));
        debug_assert_eq!(b.len(), Self::coefficient_count(b_order));
        Self {
            a_order,
            b_order,
            a,
            b,
        }
    }

    /// Number of stored coefficients for one axis: every `(p, q)` with
    /// `2 <= p+q <= order`.
    pub fn coefficient_count(order: usize) -> usize {
        let triangular = (order + 1) * (order + 2) / 2;
        triangular.saturating_sub(3)
    }

    fn terms(order: usize) -> Vec<(usize, usize)> {
        let mut terms = Vec::with_capacity(Self::coefficient_count(order));
        for p in 0..=order {
            for q in 0..=order - p {
                if p + q >= 2 {
                    terms.push((p, q));
                }
            }
        }
        terms
    }

    pub fn a_pq(&self, p: usize, q: usize) -> Option<f64> {
        Self::terms(self.a_order)
            .iter()
            .position(|&(pp, qq)| pp == p && qq == q)
            .map(|index| self.a[index])
    }

    pub fn b_pq(&self, p: usize, q: usize) -> Option<f64> {
        Self::terms(self.b_order)
            .iter()
            .position(|&(pp, qq)| pp == p && qq == q)
            .map(|index| self.b[index])
    }

    pub fn a_terms(&self) -> Vec<(usize, usize, f64)> {
        Self::terms(self.a_order)
            .into_iter()
            .zip(self.a.iter().copied())
            .map(|((p, q), value)| (p, q, value))
            .collect()
    }

    pub fn b_terms(&self) -> Vec<(usize, usize, f64)> {
        Self::terms(self.b_order)
            .into_iter()
            .zip(self.b.iter().copied())
            .map(|((p, q), value)| (p, q, value))
            .collect()
    }

    pub fn evaluate_a(&self, u: f64, v: f64) -> f64 {
        Self::terms(self.a_order)
            .into_iter()
            .zip(self.a.iter().copied())
            .map(|((p, q), coefficient)| coefficient * u.powi(p as i32) * v.powi(q as i32))
            .sum()
    }

    pub fn evaluate_b(&self, u: f64, v: f64) -> f64 {
        Self::terms(self.b_order)
            .into_iter()
            .zip(self.b.iter().copied())
            .map(|((p, q), coefficient)| coefficient * u.powi(p as i32) * v.powi(q as i32))
            .sum()
    }

    fn evaluate_a_with_derivatives(&self, u: f64, v: f64) -> (f64, f64, f64) {
        let mut value = 0.0;
        let mut du = 0.0;
        let mut dv = 0.0;
        for ((p, q), coefficient) in Self::terms(self.a_order)
            .into_iter()
            .zip(self.a.iter().copied())
        {
            let u_p = u.powi(p as i32);
            let v_q = v.powi(q as i32);
            value += coefficient * u_p * v_q;
            if p > 0 {
                du += coefficient * p as f64 * u.powi(p as i32 - 1) * v_q;
            }
            if q > 0 {
                dv += coefficient * u_p * q as f64 * v.powi(q as i32 - 1);
            }
        }
        (value, du, dv)
    }

    fn evaluate_b_with_derivatives(&self, u: f64, v: f64) -> (f64, f64, f64) {
        let mut value = 0.0;
        let mut du = 0.0;
        let mut dv = 0.0;
        for ((p, q), coefficient) in Self::terms(self.b_order)
            .into_iter()
            .zip(self.b.iter().copied())
        {
            let u_p = u.powi(p as i32);
            let v_q = v.powi(q as i32);
            value += coefficient * u_p * v_q;
            if p > 0 {
                du += coefficient * p as f64 * u.powi(p as i32 - 1) * v_q;
            }
            if q > 0 {
                dv += coefficient * u_p * q as f64 * v.powi(q as i32 - 1);
            }
        }
        (value, du, dv)
    }

    /// Re-express coefficients stored in the normalized basis
    /// `u_n = u / scale`, `v_n = v / scale` in the FITS unnormalized basis
    /// `u = x - crpix1`, `v = y - crpix2`.
    ///
    /// `A_{p,q} = A'_{p,q} / scale^(p+q)`, with the same algebra for `B`.
    pub fn to_unnormalized(&self, scale: f64) -> Self {
        Self {
            a_order: self.a_order,
            b_order: self.b_order,
            a: Self::coefficients_to_unnormalized(scale, self.a_order, &self.a),
            b: Self::coefficients_to_unnormalized(scale, self.b_order, &self.b),
        }
    }

    /// Re-express FITS unnormalized coefficients in the normalized basis
    /// `u_n = u / scale`, `v_n = v / scale`.
    ///
    /// `A'_{p,q} = A_{p,q} * scale^(p+q)`, with the same algebra for `B`.
    pub fn to_normalized(&self, scale: f64) -> Self {
        Self {
            a_order: self.a_order,
            b_order: self.b_order,
            a: Self::coefficients_to_normalized(scale, self.a_order, &self.a),
            b: Self::coefficients_to_normalized(scale, self.b_order, &self.b),
        }
    }

    fn coefficients_to_unnormalized(scale: f64, order: usize, coefficients: &[f64]) -> Vec<f64> {
        Self::terms(order)
            .into_iter()
            .zip(coefficients.iter().copied())
            .map(|((p, q), coefficient)| coefficient / scale.powi((p + q) as i32))
            .collect()
    }

    fn coefficients_to_normalized(scale: f64, order: usize, coefficients: &[f64]) -> Vec<f64> {
        Self::terms(order)
            .into_iter()
            .zip(coefficients.iter().copied())
            .map(|((p, q), coefficient)| coefficient * scale.powi((p + q) as i32))
            .collect()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Wcs {
    pub crpix1: f64,
    pub crpix2: f64,
    pub crval1: f64,
    pub crval2: f64,
    pub cd1_1: f64,
    pub cd1_2: f64,
    pub cd2_1: f64,
    pub cd2_2: f64,
    pub image_width: u32,
    pub image_height: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sip: Option<SipDistortion>,
}

#[allow(dead_code)]
impl Wcs {
    pub fn pixel_to_sky(&self, x: f64, y: f64) -> (f64, f64) {
        let dx = x - self.crpix1;
        let dy = y - self.crpix2;

        let (xi, eta) = self.intermediate_world(dx, dy);
        let xi = xi.to_radians();
        let eta = eta.to_radians();
        let ra0 = self.crval1.to_radians();
        let dec0 = self.crval2.to_radians();
        let denominator = dec0.cos() - eta * dec0.sin();
        let ra = ra0 + xi.atan2(denominator);
        let dec =
            (dec0.sin() + eta * dec0.cos()).atan2((denominator * denominator + xi * xi).sqrt());

        (ra.to_degrees().rem_euclid(360.0), dec.to_degrees())
    }

    pub fn sky_to_pixel(&self, ra: f64, dec: f64) -> (f64, f64) {
        let ra = ra.to_radians();
        let dec = dec.to_radians();
        let ra0 = self.crval1.to_radians();
        let dec0 = self.crval2.to_radians();
        let dra = ra - ra0;
        let projection_denominator = dec.sin() * dec0.sin() + dec.cos() * dec0.cos() * dra.cos();
        if projection_denominator <= 0.0 {
            return (f64::NAN, f64::NAN);
        }
        let xi = dec.cos() * dra.sin() / projection_denominator;
        let eta =
            (dec.sin() * dec0.cos() - dec.cos() * dec0.sin() * dra.cos()) / projection_denominator;
        let xi = xi.to_degrees();
        let eta = eta.to_degrees();

        let det = self.cd1_1 * self.cd2_2 - self.cd1_2 * self.cd2_1;
        if det.abs() < 1e-15 {
            return (f64::NAN, f64::NAN);
        }

        let inv_cd1_1 = self.cd2_2 / det;
        let inv_cd1_2 = -self.cd1_2 / det;
        let inv_cd2_1 = -self.cd2_1 / det;
        let inv_cd2_2 = self.cd1_1 / det;

        if self.sip.is_none() {
            let x = self.crpix1 + inv_cd1_1 * xi + inv_cd1_2 * eta;
            let y = self.crpix2 + inv_cd2_1 * xi + inv_cd2_2 * eta;
            return (x, y);
        }

        // No AP/BP inverse polynomials are stored. WCSLIB and Astropy compute
        // the inverse iteratively, so Newton's method on (u, v) starting from
        // the linear solution keeps the inverse consistent with the forward
        // model instead of relying on a second, independently fitted set.
        let mut u = inv_cd1_1 * xi + inv_cd1_2 * eta;
        let mut v = inv_cd2_1 * xi + inv_cd2_2 * eta;
        let mut converged = false;
        for _ in 0..30 {
            let (model_xi, model_eta, j11, j12, j21, j22) =
                self.intermediate_world_with_jacobian(u, v);
            let residual_xi = model_xi - xi;
            let residual_eta = model_eta - eta;
            if residual_xi.abs() < 1.0e-10 && residual_eta.abs() < 1.0e-10 {
                converged = true;
                break;
            }
            let jacobian_det = j11 * j22 - j12 * j21;
            if jacobian_det.abs() < 1.0e-15 {
                return (f64::NAN, f64::NAN);
            }
            let du = (j22 * residual_xi - j12 * residual_eta) / jacobian_det;
            let dv = (-j21 * residual_xi + j11 * residual_eta) / jacobian_det;
            u -= du;
            v -= dv;
            if du.abs() < 1.0e-10 && dv.abs() < 1.0e-10 {
                break;
            }
        }
        if !converged {
            return (f64::NAN, f64::NAN);
        }
        (self.crpix1 + u, self.crpix2 + v)
    }

    pub fn contains_sky(&self, ra: f64, dec: f64) -> bool {
        let (x, y) = self.sky_to_pixel(ra, dec);
        x >= 0.0 && x < self.image_width as f64 && y >= 0.0 && y < self.image_height as f64
    }

    pub fn pixel_scale(&self) -> f64 {
        let scale_x = (self.cd1_1.powi(2) + self.cd2_1.powi(2)).sqrt();
        let scale_y = (self.cd1_2.powi(2) + self.cd2_2.powi(2)).sqrt();
        ((scale_x + scale_y) / 2.0 * 3600.0).abs()
    }

    pub fn rotation(&self) -> f64 {
        self.cd1_1.atan2(self.cd2_1).to_degrees()
    }

    fn intermediate_world(&self, u: f64, v: f64) -> (f64, f64) {
        match &self.sip {
            None => (
                self.cd1_1 * u + self.cd1_2 * v,
                self.cd2_1 * u + self.cd2_2 * v,
            ),
            Some(sip) => {
                let focal_x = u + sip.evaluate_a(u, v);
                let focal_y = v + sip.evaluate_b(u, v);
                (
                    self.cd1_1 * focal_x + self.cd1_2 * focal_y,
                    self.cd2_1 * focal_x + self.cd2_2 * focal_y,
                )
            }
        }
    }

    fn intermediate_world_with_jacobian(&self, u: f64, v: f64) -> (f64, f64, f64, f64, f64, f64) {
        let Some(sip) = &self.sip else {
            return (
                self.cd1_1 * u + self.cd1_2 * v,
                self.cd2_1 * u + self.cd2_2 * v,
                self.cd1_1,
                self.cd1_2,
                self.cd2_1,
                self.cd2_2,
            );
        };
        let (a, a_u, a_v) = sip.evaluate_a_with_derivatives(u, v);
        let (b, b_u, b_v) = sip.evaluate_b_with_derivatives(u, v);
        let focal_x = u + a;
        let focal_y = v + b;
        let xi = self.cd1_1 * focal_x + self.cd1_2 * focal_y;
        let eta = self.cd2_1 * focal_x + self.cd2_2 * focal_y;
        let j11 = self.cd1_1 * (1.0 + a_u) + self.cd1_2 * b_u;
        let j12 = self.cd1_1 * a_v + self.cd1_2 * (1.0 + b_v);
        let j21 = self.cd2_1 * (1.0 + a_u) + self.cd2_2 * b_u;
        let j22 = self.cd2_1 * a_v + self.cd2_2 * (1.0 + b_v);
        (xi, eta, j11, j12, j21, j22)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_wcs() -> Wcs {
        Wcs {
            crpix1: 1024.0,
            crpix2: 768.0,
            crval1: 359.9,
            crval2: 45.0,
            cd1_1: -0.8 / 3600.0,
            cd1_2: 0.1 / 3600.0,
            cd2_1: 0.1 / 3600.0,
            cd2_2: 0.8 / 3600.0,
            image_width: 2048,
            image_height: 1536,
            sip: None,
        }
    }

    fn sip_test_wcs() -> Wcs {
        Wcs {
            crpix1: 1024.0,
            crpix2: 768.0,
            crval1: 359.9,
            crval2: 45.0,
            cd1_1: -0.8 / 3600.0,
            cd1_2: 0.1 / 3600.0,
            cd2_1: 0.1 / 3600.0,
            cd2_2: 0.8 / 3600.0,
            image_width: 2048,
            image_height: 1536,
            sip: Some(SipDistortion::new(
                3,
                3,
                vec![
                    0.8e-10, -0.3e-13, -1.0e-10, 0.6e-13, 2.5e-10, -0.5e-13, 1.0e-13,
                ],
                vec![
                    2.1e-10, -0.7e-13, 1.4e-10, 0.2e-13, -0.6e-10, -0.8e-13, 0.4e-13,
                ],
            )),
        }
    }

    #[test]
    fn tan_projection_round_trips_pixels() {
        let wcs = test_wcs();
        for (x, y) in [
            (0.0, 0.0),
            (1024.0, 768.0),
            (1900.25, 1200.75),
            (0.0, 1536.0),
            (2048.0, 0.0),
            (2048.0, 1536.0),
        ] {
            let (ra, dec) = wcs.pixel_to_sky(x, y);
            let (roundtrip_x, roundtrip_y) = wcs.sky_to_pixel(ra, dec);
            assert!((roundtrip_x - x).abs() < 0.05);
            assert!((roundtrip_y - y).abs() < 0.05);
        }
    }

    #[test]
    fn right_ascension_wraps_at_zero() {
        let wcs = test_wcs();
        let (ra, _) = wcs.pixel_to_sky(0.0, 768.0);
        assert!((0.0..360.0).contains(&ra));
    }

    #[test]
    fn sip_round_trips_pixels_across_full_frame() {
        let wcs = sip_test_wcs();
        for x in (0..=2048).step_by(256) {
            for y in (0..=1536).step_by(192) {
                let (ra, dec) = wcs.pixel_to_sky(x as f64, y as f64);
                let (roundtrip_x, roundtrip_y) = wcs.sky_to_pixel(ra, dec);
                assert!(
                    (roundtrip_x - x as f64).abs() < 1.0e-6,
                    "x={x} y={y} roundtrip x={roundtrip_x}"
                );
                assert!(
                    (roundtrip_y - y as f64).abs() < 1.0e-6,
                    "x={x} y={y} roundtrip y={roundtrip_y}"
                );
            }
        }
    }

    #[test]
    fn normalized_coefficients_match_unnormalized_evaluation() {
        let scale = 1024.0;
        let crpix1 = 1024.0;
        let crpix2 = 768.0;
        let a_normalized = vec![1.2e-3, -0.4e-3, 0.7e-3, 0.9e-4, -0.2e-4, 0.5e-4, -0.3e-4];
        let b_normalized = vec![-0.5e-3, 0.8e-3, 1.1e-3, 0.3e-4, -0.6e-4, 0.2e-4, -0.4e-4];
        let normalized = SipDistortion::new(3, 3, a_normalized.clone(), b_normalized.clone());
        let unnormalized = normalized.to_unnormalized(scale);

        let terms = sip_terms(3);
        for x in (0..=2048).step_by(256) {
            for y in (0..=1536).step_by(192) {
                let u = x as f64 - crpix1;
                let v = y as f64 - crpix2;
                let mut expected_a = 0.0;
                let mut expected_b = 0.0;
                for (index, &(p, q)) in terms.iter().enumerate() {
                    let u_n = u / scale;
                    let v_n = v / scale;
                    expected_a += a_normalized[index] * u_n.powi(p as i32) * v_n.powi(q as i32);
                    expected_b += b_normalized[index] * u_n.powi(p as i32) * v_n.powi(q as i32);
                }
                assert!(
                    (unnormalized.evaluate_a(u, v) - expected_a).abs()
                        < 1.0e-12 * expected_a.abs().max(1.0)
                );
                assert!(
                    (unnormalized.evaluate_b(u, v) - expected_b).abs()
                        < 1.0e-12 * expected_b.abs().max(1.0)
                );
            }
        }

        let roundtrip = unnormalized.to_normalized(scale);
        for (index, value) in a_normalized.iter().enumerate() {
            assert!((roundtrip.a[index] - value).abs() < 1.0e-12 * value.abs().max(1.0));
        }
        for (index, value) in b_normalized.iter().enumerate() {
            assert!((roundtrip.b[index] - value).abs() < 1.0e-12 * value.abs().max(1.0));
        }
    }

    #[test]
    fn sip_golden_pixel_to_sky_matches_first_principles_expansion() {
        // Golden WCS with a diagonal CD matrix (1 arcsec/px in xi and eta) so
        // the polynomial and projection are trivial to expand by hand.
        let wcs = Wcs {
            crpix1: 1000.0,
            crpix2: 500.0,
            crval1: 10.0,
            crval2: 20.0,
            cd1_1: 1.0 / 3600.0,
            cd1_2: 0.0,
            cd2_1: 0.0,
            cd2_2: 1.0 / 3600.0,
            image_width: 2048,
            image_height: 1536,
            sip: Some(SipDistortion::new(
                2,
                2,
                // Row-major storage: A02, A11, A20.
                vec![-3.0e-7, 2.0e-7, 1.0e-6],
                // Row-major storage: B02, B11, B20.
                vec![4.0e-7, 1.0e-6, -2.0e-7],
            )),
        };

        // Independent first-principles evaluation at one point. The SIP
        // polynomial is expanded explicitly here with the hardcoded
        // coefficients (A20=1e-6, A11=2e-7, A02=-3e-7, B20=-2e-7, B11=1e-6,
        // B02=4e-7); constant and linear terms are absent by construction.
        // Per the FITS SIP convention the A/B terms are pixel-unit offsets
        // added to (u, v) BEFORE the CD matrix maps to (xi, eta).
        let x = 1200.0;
        let y = 600.0;
        let u = x - wcs.crpix1;
        let v = y - wcs.crpix2;
        let focal_x = u + 1.0e-6 * u.powi(2) + 2.0e-7 * u * v + -3.0e-7 * v.powi(2);
        let focal_y = v + -2.0e-7 * u.powi(2) + 1.0e-6 * u * v + 4.0e-7 * v.powi(2);
        let xi_deg = wcs.cd1_1 * focal_x + wcs.cd1_2 * focal_y;
        let eta_deg = wcs.cd2_1 * focal_x + wcs.cd2_2 * focal_y;

        let ra0 = wcs.crval1.to_radians();
        let dec0 = wcs.crval2.to_radians();
        let xi = xi_deg.to_radians();
        let eta = eta_deg.to_radians();
        let denominator = dec0.cos() - eta * dec0.sin();
        let expected_ra = (ra0 + xi.atan2(denominator)).to_degrees().rem_euclid(360.0);
        let expected_dec = (dec0.sin() + eta * dec0.cos())
            .atan2((denominator * denominator + xi * xi).sqrt())
            .to_degrees();

        let (ra, dec) = wcs.pixel_to_sky(x, y);
        assert!(
            (ra - expected_ra).abs() < 1.0e-12,
            "ra={ra} expected={expected_ra}"
        );
        assert!(
            (dec - expected_dec).abs() < 1.0e-12,
            "dec={dec} expected={expected_dec}"
        );
    }

    #[test]
    fn sip_forward_matches_astropy_wcslib_reference() {
        // Case A header, evaluated independently with Astropy 8.0.1 / WCSLIB
        // all_pix2world(..., origin=0) on CTYPE1=RA---TAN-SIP,
        // CTYPE2=DEC--TAN-SIP, CRPIX=(crpix+1), CD = diag(1/3600),
        // A_ORDER=B_ORDER=2 and the A_p_q/B_p_q keywords below. These numbers
        // are NOT derived from this implementation.
        let wcs = Wcs {
            crpix1: 1000.0,
            crpix2: 500.0,
            crval1: 10.0,
            crval2: 20.0,
            cd1_1: 1.0 / 3600.0,
            cd1_2: 0.0,
            cd2_1: 0.0,
            cd2_2: 1.0 / 3600.0,
            image_width: 2048,
            image_height: 1536,
            sip: Some(SipDistortion::new(
                2,
                2,
                // Row-major storage: A02, A11, A20.
                vec![-3.0e-7, 2.0e-7, 1.0e-6],
                // Row-major storage: B02, B11, B20.
                vec![4.0e-7, 1.0e-6, -2.0e-7],
            )),
        };

        let cases = [
            (1200.0, 600.0, 10.059143524162279, 20.027772398027473),
            (0.0, 0.0, 9.704_960_769_923_34, 19.860_979_763_037_61),
            (2048.0, 1536.0, 10.310653223062518, 20.287_861_279_899_95),
            (500.0, 1200.0, 9.852024888942859, 20.194325123490483),
        ];
        let tolerance_arcsec = 1.0e-9;
        for (x, y, expected_ra, expected_dec) in cases {
            let (ra, dec) = wcs.pixel_to_sky(x, y);
            assert!(
                (ra - expected_ra).abs() * 3600.0 < tolerance_arcsec,
                "x={x} y={y} ra={ra} expected={expected_ra}"
            );
            assert!(
                (dec - expected_dec).abs() * 3600.0 < tolerance_arcsec,
                "x={x} y={y} dec={dec} expected={expected_dec}"
            );
        }
    }

    #[test]
    fn linear_wcs_serializes_without_sip_data() {
        let wcs = test_wcs();
        let json = serde_json::to_value(&wcs).unwrap();
        assert!(json.get("sip").is_none());
        assert!(json.get("cd1_1").is_some());
        assert!(json.get("crpix1").is_some());
    }

    fn sip_terms(order: usize) -> Vec<(usize, usize)> {
        (0..=order)
            .flat_map(|p| (0..=order - p).map(move |q| (p, q)))
            .filter(|&(p, q)| p + q >= 2)
            .collect()
    }
}
