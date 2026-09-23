use crate::ades::{AdesContext, AdesObservation, AdesRequest};
use crate::measurement::normalize_tracklet_designation;
use serde::{Deserialize, Serialize};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportFormat {
    Ades2022Psv,
    Mpc1992_80Column,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ObjectIdentity {
    Permanent(String),
    Provisional(String),
    Tracklet(String),
}

/// Format-neutral immutable observation snapshot. It describes what SkyEye
/// measured; output-specific column names and packing belong to the writers.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReportObservation {
    pub identity: ObjectIdentity,
    pub mode: String,
    pub obs_time_utc: String,
    pub ra_deg: f64,
    pub dec_deg: f64,
    pub ra_uncertainty_arcsec: Option<f64>,
    pub dec_uncertainty_arcsec: Option<f64>,
    pub astrometric_catalog: String,
    pub magnitude: Option<f64>,
    pub magnitude_uncertainty: Option<f64>,
    pub band: Option<String>,
    pub filter: Option<String>,
    pub photometric_catalog: Option<String>,
    pub aperture_arcsec: Option<f64>,
    pub snr: Option<f64>,
    pub seeing_arcsec: Option<f64>,
    pub exposure_seconds: Option<f64>,
    pub rms_fit_arcsec: Option<f64>,
    pub astrometric_reference_stars: Option<usize>,
    pub accepted_wcs: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReportContext {
    pub observatory_code: String,
    pub submitter: String,
    pub observers: Vec<String>,
    pub measurers: Vec<String>,
    pub telescope: Option<String>,
    pub telescope_aperture_m: Option<f64>,
    pub detector: Option<String>,
    pub software_version: String,
    pub position_precision_1e6_deg: bool,
    pub magnitude_precision_hundredth: bool,
    pub mpcorb_sha256: Option<String>,
    pub refcat2_sha256: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReportRequest {
    pub format: ReportFormat,
    pub context: ReportContext,
    pub observations: Vec<ReportObservation>,
}

/// A rendered report plus the non-blocking advisories collected while writing it.
///
/// Warnings never block an export: the MPC stays the authority on whether a
/// submission is acceptable, and a local heuristic must not overrule it.
#[derive(Debug, Clone, Serialize)]
pub struct RenderedReport {
    pub content: String,
    pub warnings: Vec<String>,
}

/// Magnitude bands the MPC currently accepts for new submissions. The
/// formerly-used `C` band ("clear"/no filter) was retired for new submissions.
pub const MPC_ACCEPTED_BANDS: [&str; 22] = [
    "B", "V", "R", "I", "J", "W", "U", "L", "H", "K", "Y", "G", "g", "r", "i", "w", "y", "z", "o",
    "c", "v", "u",
];

pub trait ObservationReportWriter {
    fn extension(&self) -> &'static str;
    fn render(
        &self,
        context: &ReportContext,
        observations: &[ReportObservation],
    ) -> Result<RenderedReport, Vec<String>>;
}
pub struct AdesPsvWriter;
pub struct Mpc80Writer;
impl ObservationReportWriter for AdesPsvWriter {
    fn extension(&self) -> &'static str {
        "psv"
    }
    fn render(
        &self,
        c: &ReportContext,
        rows: &[ReportObservation],
    ) -> Result<RenderedReport, Vec<String>> {
        crate::ades::render(&to_ades(c, rows))
    }
}
impl ObservationReportWriter for Mpc80Writer {
    fn extension(&self) -> &'static str {
        "txt"
    }
    fn render(
        &self,
        c: &ReportContext,
        rows: &[ReportObservation],
    ) -> Result<RenderedReport, Vec<String>> {
        render_mpc80(c, rows)
    }
}
pub fn writer(format: ReportFormat) -> Box<dyn ObservationReportWriter> {
    match format {
        ReportFormat::Ades2022Psv => Box::new(AdesPsvWriter),
        ReportFormat::Mpc1992_80Column => Box::new(Mpc80Writer),
    }
}

fn to_ades(c: &ReportContext, rows: &[ReportObservation]) -> AdesRequest {
    AdesRequest {
        context: AdesContext {
            observatory_code: c.observatory_code.clone(),
            submitter: c.submitter.clone(),
            observers: c.observers.clone(),
            measurers: c.measurers.clone(),
            telescope: c.telescope.clone(),
            telescope_aperture_m: c.telescope_aperture_m,
            detector: c.detector.clone(),
            software_version: c.software_version.clone(),
            position_precision_1e6_deg: c.position_precision_1e6_deg,
            magnitude_precision_hundredth: c.magnitude_precision_hundredth,
            mpcorb_sha256: c.mpcorb_sha256.clone(),
            refcat2_sha256: c.refcat2_sha256.clone(),
        },
        observations: rows
            .iter()
            .map(|o| {
                let (perm_id, prov_id, trk_sub) = match &o.identity {
                    ObjectIdentity::Permanent(v) => (Some(v.clone()), None, None),
                    ObjectIdentity::Provisional(v) => (None, Some(v.clone()), None),
                    ObjectIdentity::Tracklet(v) => (None, None, Some(v.clone())),
                };
                AdesObservation {
                    perm_id,
                    prov_id,
                    trk_sub,
                    mode: o.mode.clone(),
                    obs_time: o.obs_time_utc.clone(),
                    ra_deg: o.ra_deg,
                    dec_deg: o.dec_deg,
                    rms_ra_arcsec: o.ra_uncertainty_arcsec,
                    rms_dec_arcsec: o.dec_uncertainty_arcsec,
                    ast_cat: o.astrometric_catalog.clone(),
                    mag: o.magnitude,
                    rms_mag: o.magnitude_uncertainty,
                    band: o.band.clone(),
                    filter: o.filter.clone(),
                    phot_cat: o.photometric_catalog.clone(),
                    phot_ap_arcsec: o.aperture_arcsec,
                    snr: o.snr,
                    seeing_arcsec: o.seeing_arcsec,
                    exposure_seconds: o.exposure_seconds,
                    rms_fit_arcsec: o.rms_fit_arcsec,
                    astrometric_reference_stars: o.astrometric_reference_stars,
                    accepted_wcs: o.accepted_wcs,
                }
            })
            .collect(),
    }
}

fn render_mpc80(
    c: &ReportContext,
    rows: &[ReportObservation],
) -> Result<RenderedReport, Vec<String>> {
    let stn = c.observatory_code.trim();
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    if stn.eq_ignore_ascii_case("XXX") {
        errors.push("MPC 台站代码仍为占位值 XXX".into());
    } else if stn.len() != 3 || !stn.is_ascii() {
        errors.push("MPC 80-column 要求 3 字符台站代码".into());
    }
    if rows.is_empty() {
        errors.push("没有可导出的观测".into());
    }
    let mut lines = vec![format!("COD {stn}")];
    append_header_names(&mut lines, "OBS", &c.observers, &mut errors, &mut warnings);
    append_header_names(&mut lines, "MEA", &c.measurers, &mut errors, &mut warnings);
    if let Some(telescope) = c
        .telescope
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        let line = format!("TEL {}", telescope.trim());
        if !line.is_ascii() || line.len() > 80 {
            errors.push("TEL 报告头必须是最多 80 字符的 ASCII 文本".into());
        } else {
            lines.push(line);
        }
    } else {
        errors.push("MPC 80-column 缺少 TEL 台站仪器信息".into());
    }
    lines.push(format!(
        "ACK MPCReport file updated {}",
        mpc_report_timestamp()
    ));
    match mpc_catalog_header(rows) {
        Ok(catalog) => lines.push(format!("NET {catalog}")),
        Err(error) => errors.push(error),
    }
    for (i, o) in rows.iter().enumerate() {
        match mpc80_line(o, stn) {
            Ok(v) => lines.push(v),
            Err(e) => errors.push(format!("第 {} 条：{e}", i + 1)),
        }
        if o.magnitude.is_some() {
            if let Some(band) = o.band.as_deref() {
                if !MPC_ACCEPTED_BANDS.contains(&band) {
                    warnings.push(format!(
                        "第 {} 条：波段 {band} 不在 MPC 当前接受的波段表内（C 已不再接受新提交）",
                        i + 1
                    ));
                }
            }
        }
    }
    if errors.is_empty() {
        lines.push("----- end -----".into());
        Ok(RenderedReport {
            content: lines.join("\r\n") + "\r\n",
            warnings,
        })
    } else {
        Err(errors)
    }
}

/// MPC expects names as "initials + surname" (`J. M. Doe`), never an all-caps
/// form or a full given name. These are advisories only: the MPC may still
/// accept a name we cannot parse confidently, so we never block on them.
fn name_style_warnings(keyword: &str, name: &str) -> Vec<String> {
    let mut warnings = Vec::new();
    let letters: Vec<char> = name.chars().filter(|c| c.is_ascii_alphabetic()).collect();
    if letters.len() > 1 && letters.iter().all(|c| c.is_ascii_uppercase()) {
        warnings.push(format!(
            "{keyword} 姓名「{name}」疑似全大写，MPC 要求写作「缩写. 姓」"
        ));
    }
    if name
        .char_indices()
        .any(|(i, c)| c == '.' && name[i + 1..].chars().next().is_some_and(|next| next != ' '))
    {
        warnings.push(format!(
            "{keyword} 姓名「{name}」的缩写之间缺少空格，MPC 要求写作「J. M. Doe」"
        ));
    }
    let first = name.split_whitespace().next().unwrap_or_default();
    if first.len() > 1 && !first.ends_with('.') {
        warnings.push(format!(
            "{keyword} 姓名「{name}」疑似使用全名而非缩写首字母，MPC 要求写作「J. Doe」"
        ));
    }
    warnings
}

fn append_header_names(
    lines: &mut Vec<String>,
    keyword: &str,
    names: &[String],
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    if names.is_empty() {
        errors.push(format!("MPC 80-column 缺少 {keyword} 信息"));
        return;
    }
    let mut line = format!("{keyword} ");
    for name in names {
        let name = name.trim();
        if name.is_empty() || !name.is_ascii() || name.len() + 4 > 80 {
            errors.push(format!(
                "{keyword} 姓名必须是可放入 80 字符报告头的 ASCII 文本"
            ));
            return;
        }
        warnings.extend(name_style_warnings(keyword, name));
        let separator = if line.len() == 4 { "" } else { ", " };
        if line.len() + separator.len() + name.len() > 80 {
            lines.push(line);
            line = format!("{keyword} {name}");
        } else {
            line.push_str(separator);
            line.push_str(name);
        }
    }
    lines.push(line);
}

fn mpc_report_timestamp() -> String {
    let offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
    let now = OffsetDateTime::now_utc().to_offset(offset);
    format!(
        "{:04}.{:02}.{:02} {:02}:{:02}:{:02}",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    )
}

fn mpc_catalog_header(rows: &[ReportObservation]) -> Result<&'static str, String> {
    let mut catalogs = rows.iter().map(|row| row.astrometric_catalog.trim());
    let first = catalogs.next().ok_or("没有可导出的观测")?;
    if catalogs.any(|catalog| catalog != first) {
        return Err("同一 MPC 80-column 报告不能混用不同天文参考星表".into());
    }
    match first {
        "Gaia3" | "Gaia-DR3" | "Gaia DR3" => Ok("Gaia-DR3"),
        "Gaia2" | "Gaia-DR2" | "Gaia DR2" => Ok("Gaia-DR2"),
        "Gaia1" | "Gaia-DR1" | "Gaia DR1" => Ok("Gaia-DR1"),
        _ => Err(format!("MPC 80-column 不支持天文参考星表 {first}")),
    }
}

fn mpc80_line(o: &ReportObservation, stn: &str) -> Result<String, String> {
    if !o.accepted_wcs {
        return Err("WCS 未接受".into());
    }
    let id = pack_identity(&o.identity)?;
    let date = mpc_date(&o.obs_time_utc)?;
    let ra = format_ra(o.ra_deg)?;
    let dec = format_dec(o.dec_deg)?;
    let (mag, band) = match o.magnitude {
        Some(m) => {
            let b = o.band.as_deref().ok_or("星等缺少波段")?;
            if b.len() != 1 || !b.is_ascii() {
                return Err("80-column 波段必须为一个 ASCII 字符".into());
            }
            let value = format!("{m:<5.1}");
            if value.len() != 5 {
                return Err("星等超出 MPC 80-column 字段范围".into());
            }
            (value, b)
        }
        None => ("     ".into(), " "),
    };
    let line = format!("{id}  C{date}{ra}{dec}         {mag}{band}      {stn}");
    if line.len() != 80 {
        return Err(format!("内部列宽错误：{} 字节", line.len()));
    }
    Ok(line)
}
fn pack_identity(id: &ObjectIdentity) -> Result<String, String> {
    let raw = match id {
        ObjectIdentity::Permanent(v)
        | ObjectIdentity::Provisional(v)
        | ObjectIdentity::Tracklet(v) => v.trim(),
    };
    if raw.is_empty() || !raw.is_ascii() {
        return Err("designation 必须是非空 ASCII".into());
    }
    match id {
        ObjectIdentity::Permanent(_) => {
            let n = raw.parse::<u32>().map_err(|_| "永久编号必须是数字")?;
            let packed = if n <= 99_999 {
                format!("{n:05}")
            } else if n <= 619_999 {
                let q = n / 10_000;
                let c = match q {
                    10..=35 => (b'A' + (q - 10) as u8) as char,
                    36..=61 => (b'a' + (q - 36) as u8) as char,
                    _ => return Err("永久编号无法打包".into()),
                };
                format!("{c}{:04}", n % 10_000)
            } else {
                return Err("永久编号超过经典 packed-number 范围；请改用 ADES".into());
            };
            Ok(format!("{packed}       "))
        }
        ObjectIdentity::Provisional(_) => pack_provisional(raw),
        ObjectIdentity::Tracklet(_) => {
            let tracklet = normalize_tracklet_designation(raw)?;
            Ok(format!("     {tracklet:<7}"))
        }
    }
}
fn pack_provisional(raw: &str) -> Result<String, String> {
    let p: Vec<_> = raw.split_whitespace().collect();
    if p.len() != 2 || p[0].len() != 4 || p[1].len() < 2 {
        return Err("临时编号应形如 2023 AB".into());
    }
    let year = p[0].parse::<u16>().map_err(|_| "无效年份")?;
    let century = match year / 100 {
        18 => 'I',
        19 => 'J',
        20 => 'K',
        21 => 'L',
        _ => return Err("年份无法打包".into()),
    };
    let s = p[1].as_bytes();
    let cycle = if s.len() == 2 {
        0
    } else {
        std::str::from_utf8(&s[1..s.len() - 1])
            .ok()
            .and_then(|x| x.parse::<u32>().ok())
            .ok_or("循环号无效")?
    };
    if cycle > 619 {
        return Err("循环号过大".into());
    }
    let b62 = |n: u32| match n {
        0..=9 => (b'0' + n as u8) as char,
        10..=35 => (b'A' + (n - 10) as u8) as char,
        _ => (b'a' + (n - 36) as u8) as char,
    };
    Ok(format!(
        "     {century}{:02}{}{}{}{}",
        year % 100,
        s[0] as char,
        b62(cycle / 10),
        (b'0' + (cycle % 10) as u8) as char,
        s[s.len() - 1] as char
    ))
}
fn mpc_date(v: &str) -> Result<String, String> {
    let dt = OffsetDateTime::parse(v, &Rfc3339).map_err(|_| "obsTime 不是 RFC3339 UTC")?;
    let day = dt.day() as f64
        + (dt.hour() as f64 * 3600.0
            + dt.minute() as f64 * 60.0
            + dt.second() as f64
            + dt.nanosecond() as f64 / 1e9)
            / 86400.0;
    let rounded_day = (day * 1_000_000.0).round() / 1_000_000.0;
    if rounded_day >= dt.day() as f64 + 1.0 {
        let next = dt.date().next_day().ok_or("观测日期超出支持范围")?;
        return Ok(format!(
            "{:04} {:02} {:09.6}",
            next.year(),
            u8::from(next.month()),
            next.day() as f64
        ));
    }
    Ok(format!(
        "{:04} {:02} {:09.6}",
        dt.year(),
        u8::from(dt.month()),
        rounded_day
    ))
}
fn format_ra(d: f64) -> Result<String, String> {
    if !d.is_finite() || !(0.0..360.0).contains(&d) {
        return Err("RA 超出范围".into());
    }
    let total_milliseconds = ((d / 15.0) * 3_600_000.0).round() as u64 % 86_400_000;
    let hh = total_milliseconds / 3_600_000;
    let mm = total_milliseconds % 3_600_000 / 60_000;
    let seconds = (total_milliseconds % 60_000) as f64 / 1_000.0;
    Ok(format!("{hh:02} {mm:02} {seconds:06.3}"))
}
fn format_dec(d: f64) -> Result<String, String> {
    if !d.is_finite() || !(-90.0..=90.0).contains(&d) {
        return Err("Dec 超出范围".into());
    }
    let sign = if d.is_sign_negative() { '-' } else { '+' };
    let total_centiarcseconds = (d.abs() * 360_000.0).round() as u64;
    let dd = total_centiarcseconds / 360_000;
    let mm = total_centiarcseconds % 360_000 / 6_000;
    let seconds = (total_centiarcseconds % 6_000) as f64 / 100.0;
    Ok(format!("{sign}{dd:02} {mm:02} {seconds:05.2}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request() -> ReportRequest {
        ReportRequest {
            format: ReportFormat::Mpc1992_80Column,
            context: ReportContext {
                observatory_code: "F51".into(),
                submitter: "x".into(),
                observers: vec!["J. Observer".into()],
                measurers: vec!["M. Measurer".into()],
                telescope: Some("1.8-m f/4.4 reflector + CCD".into()),
                telescope_aperture_m: Some(1.8),
                detector: Some("CCD".into()),
                software_version: "x".into(),
                position_precision_1e6_deg: true,
                magnitude_precision_hundredth: false,
                mpcorb_sha256: None,
                refcat2_sha256: None,
            },
            observations: vec![ReportObservation {
                identity: ObjectIdentity::Provisional("2023 AB".into()),
                mode: "CCD".into(),
                obs_time_utc: "2023-05-16T10:29:05.222Z".into(),
                ra_deg: 239.1533625,
                dec_deg: -23.2121306,
                ra_uncertainty_arcsec: None,
                dec_uncertainty_arcsec: None,
                astrometric_catalog: "Gaia3".into(),
                magnitude: Some(21.55),
                magnitude_uncertainty: None,
                band: Some("r".into()),
                filter: None,
                photometric_catalog: Some("ATLAS2".into()),
                aperture_arcsec: None,
                snr: None,
                seeing_arcsec: None,
                exposure_seconds: None,
                rms_fit_arcsec: None,
                astrometric_reference_stars: None,
                accepted_wcs: true,
            }],
        }
    }
    #[test]
    fn emits_80_columns() {
        let r = request();
        let text = writer(r.format)
            .render(&r.context, &r.observations)
            .unwrap()
            .content;
        let observation = text.lines().find(|line| line.len() == 80).unwrap();
        assert_eq!(observation.len(), 80)
    }

    #[test]
    fn mpc80_matches_high_precision_column_layout() {
        let mut r = request();
        r.observations[0].identity = ObjectIdentity::Tracklet("YYH0001".into());
        r.observations[0].obs_time_utc = "2025-09-16T10:02:16.466548Z".into();
        r.observations[0].ra_deg = (12.0 / 60.0 + 9.970 / 3_600.0) * 15.0;
        r.observations[0].dec_deg = -(10.0 + 46.0 / 60.0 + 52.84 / 3_600.0);
        r.observations[0].magnitude = Some(19.8);
        r.observations[0].band = Some("G".into());
        let text = writer(r.format)
            .render(&r.context, &r.observations)
            .unwrap()
            .content;
        assert!(text.starts_with("COD F51\r\nOBS J. Observer\r\nMEA M. Measurer\r\n"));
        assert!(text.contains("NET Gaia-DR3\r\n"));
        assert!(text.contains(
            "     YYH0001  C2025 09 16.41824600 12 09.970-10 46 52.84         19.8 G      F51\r\n"
        ));
        assert!(text.ends_with("----- end -----\r\n"));
    }
    #[test]
    fn same_snapshot_supports_both_writers() {
        let mut r = request();
        assert!(writer(ReportFormat::Ades2022Psv)
            .render(&r.context, &r.observations)
            .is_ok());
        r.format = ReportFormat::Mpc1992_80Column;
        assert!(writer(r.format).render(&r.context, &r.observations).is_ok())
    }

    #[test]
    fn ades_accepts_gaia_uppercase_g_photometric_band() {
        let mut r = request();
        r.observations[0].band = Some("G".into());
        assert!(writer(ReportFormat::Ades2022Psv)
            .render(&r.context, &r.observations)
            .is_ok());
    }

    #[test]
    fn reports_reject_placeholder_station_code() {
        let mut r = request();
        r.context.observatory_code = "XXX".into();
        let errors = writer(r.format)
            .render(&r.context, &r.observations)
            .expect_err("placeholder station code must not be exportable");
        assert!(errors.iter().any(|error| error.contains("占位值 XXX")));

        r.format = ReportFormat::Ades2022Psv;
        let errors = writer(r.format)
            .render(&r.context, &r.observations)
            .expect_err("placeholder station code must not be exportable");
        assert!(errors.iter().any(|error| error.contains("占位值 XXX")));
    }

    #[test]
    fn ades_includes_reduction_provenance_and_configured_precision() {
        let mut r = request();
        r.context.software_version = "SkyEye 0.1.0".into();
        r.observations[0].ra_deg = 239.1533631;
        r.observations[0].dec_deg = -23.2121311;
        r.observations[0].magnitude = Some(21.56);
        r.observations[0].filter = Some("r".into());
        r.observations[0].rms_fit_arcsec = Some(0.087);
        r.observations[0].astrometric_reference_stars = Some(65);
        let text = writer(ReportFormat::Ades2022Psv)
            .render(&r.context, &r.observations)
            .unwrap()
            .content;
        assert!(text.contains("# software\n! astrometry SkyEye 0.1.0"));
        assert!(text.contains("band|fltr|photCat"));
        assert!(text.contains("exp|rmsFit|nStars"));
        assert!(text.contains("239.153363|-23.212131"));
        assert!(text.contains("21.6||r|r|ATLAS2"));
        assert!(text.contains("0.087|65"));
    }

    #[test]
    fn ades_requires_the_context_sections_the_schema_marks_mandatory() {
        let mut r = request();
        r.format = ReportFormat::Ades2022Psv;
        r.context.measurers = vec![];
        r.context.telescope = None;
        r.context.telescope_aperture_m = None;
        r.context.detector = None;
        let errors = writer(r.format)
            .render(&r.context, &r.observations)
            .expect_err("ADES must reject a context missing mandatory sections");
        assert!(errors.iter().any(|e| e.contains("measurers")));
        assert!(errors.iter().any(|e| e.contains("telescope")));
        assert!(errors.iter().any(|e| e.contains("aperture")));
        assert!(errors.iter().any(|e| e.contains("detector")));
    }

    #[test]
    fn ades_allows_an_empty_observer_list_because_the_schema_makes_it_optional() {
        let mut r = request();
        r.format = ReportFormat::Ades2022Psv;
        r.context.observers = vec![];
        assert!(writer(r.format).render(&r.context, &r.observations).is_ok());
    }

    #[test]
    fn mpc80_keeps_requiring_observers() {
        let mut r = request();
        r.context.observers = vec![];
        let errors = writer(r.format)
            .render(&r.context, &r.observations)
            .expect_err("MPC 80-column must reject an empty OBS header");
        assert!(errors.iter().any(|e| e.contains("OBS")));
    }

    #[test]
    fn retired_band_is_a_warning_not_a_blocker_for_both_writers() {
        for format in [ReportFormat::Ades2022Psv, ReportFormat::Mpc1992_80Column] {
            let mut r = request();
            r.format = format;
            r.observations[0].band = Some("C".into());
            let report = writer(r.format)
                .render(&r.context, &r.observations)
                .expect("a retired band must not block an export");
            assert!(report.warnings.iter().any(|w| w.contains("波段 C")));
        }
    }

    #[test]
    fn name_style_is_a_warning_not_a_blocker() {
        let mut r = request();
        r.context.observers = vec!["Vangelis Papathanassiou".into()];
        let report = writer(r.format)
            .render(&r.context, &r.observations)
            .expect("an odd name must not block an export");
        assert!(report.warnings.iter().any(|w| w.contains("疑似使用全名")));
    }

    #[test]
    fn ades_accepts_any_photometric_catalog_and_warns_without_one() {
        let mut r = request();
        r.format = ReportFormat::Ades2022Psv;
        r.observations[0].photometric_catalog = Some("Gaia3".into());
        let report = writer(r.format)
            .render(&r.context, &r.observations)
            .expect("the schema does not pin photCat to a single catalog");
        assert!(report.warnings.is_empty());

        r.observations[0].photometric_catalog = None;
        let report = writer(r.format)
            .render(&r.context, &r.observations)
            .expect("photCat is optional in the ADES photometry group");
        assert!(report.warnings.iter().any(|w| w.contains("photCat")));
    }
}
