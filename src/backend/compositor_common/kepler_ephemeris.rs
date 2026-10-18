//! Offline approximate heliocentric positions from NASA/JPL's Table 1 fit.
//!
//! Source: <https://ssd.jpl.nasa.gov/planets/approx_pos.html>, checked 2026-10-17.
//! Coordinates are AU in the mean ecliptic/equinox of J2000; arguments are
//! Julian dates in TDB, not local civil time. The third entry is the Earth-Moon
//! barycentre, not Earth's centre. These are low-accuracy fitted Keplerian
//! elements, not the integrated Horizons/DE ephemerides or an N-body model.
//!
//! Display code may compress each orbit with its own uniform scale, preserving
//! eccentricity, inclination and direction. It must label that display as
//! distance-compressed and use exactly the same coordinates for picking.

pub(crate) const EPHEMERIS_SOURCE_URL: &str = "https://ssd.jpl.nasa.gov/planets/approx_pos.html";
pub(crate) const J2000: f64 = 2_451_545.0;
pub(crate) const VALID_JULIAN_DATE_MIN: f64 = 2_378_496.5; // 1800-01-01
pub(crate) const VALID_JULIAN_DATE_MAX: f64 = 2_469_807.5; // 2050-01-01

const SECONDS_PER_DAY: f64 = 86_400.0;
const JULIAN_DATE_UNIX_EPOCH: f64 = 2_440_587.5;
const UNIX_SECONDS_J2000_NOON: f64 = 946_728_000.0;

#[derive(Clone, Copy)]
struct ElementFit {
    // a (AU), e, I, L, longitude of perihelion, longitude of ascending node.
    // The four angles are degrees. Rates are per Julian century of 36525 days.
    epoch: [f64; 6],
    rate: [f64; 6],
}

// JPL Table 1, unchanged decimal precision. Never mix these short-interval
// elements with the additional terms belonging to the separate long fit.
const ELEMENTS: [ElementFit; 8] = [
    ElementFit {
        epoch: [
            0.38709927,
            0.20563593,
            7.00497902,
            252.25032350,
            77.45779628,
            48.33076593,
        ],
        rate: [
            0.00000037,
            0.00001906,
            -0.00594749,
            149472.67411175,
            0.16047689,
            -0.12534081,
        ],
    },
    ElementFit {
        epoch: [
            0.72333566,
            0.00677672,
            3.39467605,
            181.97909950,
            131.60246718,
            76.67984255,
        ],
        rate: [
            0.00000390,
            -0.00004107,
            -0.00078890,
            58517.81538729,
            0.00268329,
            -0.27769418,
        ],
    },
    ElementFit {
        epoch: [
            1.00000261,
            0.01671123,
            -0.00001531,
            100.46457166,
            102.93768193,
            0.0,
        ],
        rate: [
            0.00000562,
            -0.00004392,
            -0.01294668,
            35999.37244981,
            0.32327364,
            0.0,
        ],
    },
    ElementFit {
        epoch: [
            1.52371034,
            0.09339410,
            1.84969142,
            -4.55343205,
            -23.94362959,
            49.55953891,
        ],
        rate: [
            0.00001847,
            0.00007882,
            -0.00813131,
            19140.30268499,
            0.44441088,
            -0.29257343,
        ],
    },
    ElementFit {
        epoch: [
            5.20288700,
            0.04838624,
            1.30439695,
            34.39644051,
            14.72847983,
            100.47390909,
        ],
        rate: [
            -0.00011607,
            -0.00013253,
            -0.00183714,
            3034.74612775,
            0.21252668,
            0.20469106,
        ],
    },
    ElementFit {
        epoch: [
            9.53667594,
            0.05386179,
            2.48599187,
            49.95424423,
            92.59887831,
            113.66242448,
        ],
        rate: [
            -0.00125060,
            -0.00050991,
            0.00193609,
            1222.49362201,
            -0.41897216,
            -0.28867794,
        ],
    },
    ElementFit {
        epoch: [
            19.18916464,
            0.04725744,
            0.77263783,
            313.23810451,
            170.95427630,
            74.01692503,
        ],
        rate: [
            -0.00196176,
            -0.00004397,
            -0.00242939,
            428.48202785,
            0.40805281,
            0.04240589,
        ],
    },
    ElementFit {
        epoch: [
            30.06992276,
            0.00859048,
            1.77004347,
            -55.12002969,
            44.96476227,
            131.78422574,
        ],
        rate: [
            0.00026291,
            0.00005105,
            0.00035372,
            218.45945325,
            -0.32241464,
            -0.00508664,
        ],
    },
];

fn elements(julian_date: f64, body_index: usize) -> Option<[f64; 6]> {
    if !julian_date.is_finite()
        || !(VALID_JULIAN_DATE_MIN..=VALID_JULIAN_DATE_MAX).contains(&julian_date)
    {
        return None;
    }
    let fit = ELEMENTS.get(body_index)?;
    let centuries = (julian_date - J2000) / 36_525.0;
    Some(std::array::from_fn(|i| {
        fit.epoch[i] + centuries * fit.rate[i]
    }))
}

/// Solve M = E - e sin(E), in radians, with a bounded deterministic iteration.
fn eccentric_anomaly(mean_anomaly: f64, eccentricity: f64) -> Option<f64> {
    if !mean_anomaly.is_finite() || !eccentricity.is_finite() || !(0.0..1.0).contains(&eccentricity)
    {
        return None;
    }
    let mean = (mean_anomaly + std::f64::consts::PI).rem_euclid(std::f64::consts::TAU)
        - std::f64::consts::PI;
    let mut anomaly = mean + eccentricity * mean.sin();
    for _ in 0..16 {
        let delta =
            (mean - anomaly + eccentricity * anomaly.sin()) / (1.0 - eccentricity * anomaly.cos());
        anomaly += delta;
        if delta.abs() <= 1e-13 {
            return anomaly.is_finite().then_some(anomaly);
        }
    }
    None
}

fn point_on_orbit(elements: [f64; 6], anomaly: f64) -> [f64; 3] {
    let [a, e, inclination, _, perihelion, node] = elements;
    let (sin_i, cos_i) = inclination.to_radians().sin_cos();
    let (sin_node, cos_node) = node.to_radians().sin_cos();
    let (sin_arg, cos_arg) = (perihelion - node).to_radians().sin_cos();
    let (sin_e, cos_e) = anomaly.sin_cos();
    let x = a * (cos_e - e);
    let y = a * (1.0 - e * e).sqrt() * sin_e;
    [
        (cos_arg * cos_node - sin_arg * sin_node * cos_i) * x
            + (-sin_arg * cos_node - cos_arg * sin_node * cos_i) * y,
        (cos_arg * sin_node + sin_arg * cos_node * cos_i) * x
            + (-sin_arg * sin_node + cos_arg * cos_node * cos_i) * y,
        (sin_arg * sin_i) * x + (cos_arg * sin_i) * y,
    ]
}

/// Mercury, Venus, Earth-Moon barycentre, Mars, Jupiter, Saturn, Uranus, Neptune.
/// Positions are AU in J2000 ecliptic X/Y/Z, with the Sun at the origin.
/// Dates outside the fitted interval are rejected, never silently clamped or
/// extrapolated. `julian_date_from_unix` converts supported civil timestamps.
pub(crate) fn heliocentric_positions(julian_date: f64) -> Option<[[f64; 3]; 8]> {
    let mut result = [[0.0; 3]; 8];
    for (index, position) in result.iter_mut().enumerate() {
        let elements = elements(julian_date, index)?;
        let mean_anomaly = (elements[3] - elements[4]).to_radians();
        let anomaly = eccentric_anomaly(mean_anomaly, elements[1])?;
        *position = point_on_orbit(elements, anomaly);
    }
    Some(result)
}

/// Point on the *same epoch's* fitted ellipse as `heliocentric_positions`.
/// `body_index` is 0..8 (Mercury through Neptune); eccentric anomaly is radians:
/// zero = perihelion, PI = aphelion. This is geometry, not uniform elapsed time.
/// It can provide peri/apo/transverse guide axes without a new orbital model.
pub(crate) fn orbit_position(
    julian_date: f64,
    body_index: usize,
    eccentric_anomaly: f64,
) -> Option<[f64; 3]> {
    if !eccentric_anomaly.is_finite() {
        return None;
    }
    let elements = elements(julian_date, body_index)?;
    Some(point_on_orbit(
        elements,
        eccentric_anomaly.rem_euclid(std::f64::consts::TAU),
    ))
}

pub(crate) fn semi_major_axes(julian_date: f64) -> Option<[f64; 8]> {
    let mut result = [0.0; 8];
    for (index, axis) in result.iter_mut().enumerate() {
        *axis = elements(julian_date, index)?[0];
    }
    Some(result)
}

// TAI-UTC steps and the periodic ET approximation are from NAIF's public LSK:
// https://naif.jpl.nasa.gov/pub/naif/generic_kernels/lsk/naif0012.tls
// The latest supplied step is 2017-01-01. Dates later than that use 37 seconds;
// future announced leap seconds must be added here. POSIX cannot represent the
// leap second itself, and pre-1972 rubber-second UTC is intentionally unsupported.
const TAI_MINUS_UTC: [(f64, f64); 28] = [
    (63_072_000.0, 10.0),    // 1972-01-01
    (78_796_800.0, 11.0),    // 1972-07-01
    (94_694_400.0, 12.0),    // 1973-01-01
    (126_230_400.0, 13.0),   // 1974-01-01
    (157_766_400.0, 14.0),   // 1975-01-01
    (189_302_400.0, 15.0),   // 1976-01-01
    (220_924_800.0, 16.0),   // 1977-01-01
    (252_460_800.0, 17.0),   // 1978-01-01
    (283_996_800.0, 18.0),   // 1979-01-01
    (315_532_800.0, 19.0),   // 1980-01-01
    (362_793_600.0, 20.0),   // 1981-07-01
    (394_329_600.0, 21.0),   // 1982-07-01
    (425_865_600.0, 22.0),   // 1983-07-01
    (489_024_000.0, 23.0),   // 1985-07-01
    (567_993_600.0, 24.0),   // 1988-01-01
    (631_152_000.0, 25.0),   // 1990-01-01
    (662_688_000.0, 26.0),   // 1991-01-01
    (709_948_800.0, 27.0),   // 1992-07-01
    (741_484_800.0, 28.0),   // 1993-07-01
    (773_020_800.0, 29.0),   // 1994-07-01
    (820_454_400.0, 30.0),   // 1996-01-01
    (867_715_200.0, 31.0),   // 1997-07-01
    (915_148_800.0, 32.0),   // 1999-01-01
    (1_136_073_600.0, 33.0), // 2006-01-01
    (1_230_768_000.0, 34.0), // 2009-01-01
    (1_341_100_800.0, 35.0), // 2012-07-01
    (1_435_708_800.0, 36.0), // 2015-07-01
    (1_483_228_800.0, 37.0), // 2017-01-01
];

/// Convert a finite POSIX UTC timestamp to an approximate Julian date in TDB.
/// No timezone/DST offset belongs here. TAI-UTC and TT-TAI are accounted for,
/// followed by NAIF's bounded millisecond periodic ET term. This is sufficient
/// for the much less accurate planetary fit; it is not a precision time API.
/// Pre-1972 timestamps and dates beyond the fitted interval are rejected.
pub(crate) fn julian_date_from_unix(unix: f64) -> Option<f64> {
    if !unix.is_finite() {
        return None;
    }
    let tai_minus_utc = TAI_MINUS_UTC
        .iter()
        .rev()
        .find(|(start, _)| unix >= *start)?
        .1;
    let tt_offset = tai_minus_utc + 32.184;
    let tt_seconds = unix - UNIX_SECONDS_J2000_NOON + tt_offset;
    // Evaluate the periodic correction using TT on the right-hand side.
    // Re-evaluating with ET changes it by far below a microsecond here.
    let mean = 6.239996 + 1.99096871e-7 * tt_seconds;
    let eccentric = mean + 1.671e-2 * mean.sin();
    let et_offset = tt_offset + 1.657e-3 * eccentric.sin();
    let julian_date = JULIAN_DATE_UNIX_EPOCH + (unix + et_offset) / SECONDS_PER_DAY;
    (julian_date.is_finite()
        && (VALID_JULIAN_DATE_MIN..=VALID_JULIAN_DATE_MAX).contains(&julian_date))
    .then_some(julian_date)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn length(v: [f64; 3]) -> f64 {
        v.into_iter().map(|x| x * x).sum::<f64>().sqrt()
    }

    fn difference(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
        std::array::from_fn(|i| a[i] - b[i])
    }

    #[test]
    fn dates_are_tdb_and_reject_unsupported_instants() {
        // 2000-01-01 12:00 UTC is 64.184 seconds after J2000 TT.
        let j2000_utc_noon = julian_date_from_unix(946_728_000.0).unwrap();
        assert!(((j2000_utc_noon - J2000) * SECONDS_PER_DAY - 64.184).abs() < 0.002);
        // 2017-01-01 UTC: 37 TAI-UTC + 32.184 TT-TAI seconds.
        let after_leap = julian_date_from_unix(1_483_228_800.0).unwrap();
        assert!(((after_leap - 2_457_754.5) * SECONDS_PER_DAY - 69.184).abs() < 0.002);
        // The inserted second is absent from POSIX, but still advances ET.
        let before_leap = julian_date_from_unix(1_483_228_799.0).unwrap();
        assert!(((after_leap - before_leap) * SECONDS_PER_DAY - 2.0).abs() < 0.0001);
        assert!(julian_date_from_unix(0.0).is_none());
        for date in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, f64::MAX] {
            assert!(julian_date_from_unix(date).is_none());
            assert!(heliocentric_positions(date).is_none());
        }
        assert!(heliocentric_positions(VALID_JULIAN_DATE_MIN).is_some());
        assert!(heliocentric_positions(VALID_JULIAN_DATE_MAX).is_some());
        assert!(heliocentric_positions(VALID_JULIAN_DATE_MIN - 0.001).is_none());
        assert!(heliocentric_positions(VALID_JULIAN_DATE_MAX + 0.001).is_none());
        assert!(orbit_position(J2000, 8, 0.0).is_none());
        assert!(orbit_position(J2000, 0, f64::NAN).is_none());
    }

    #[test]
    fn mercury_geometry_matches_jpl_perihelion_and_orbit_plane() {
        let peri = orbit_position(J2000, 0, 0.0).unwrap();
        let apo = orbit_position(J2000, 0, std::f64::consts::PI).unwrap();
        let quarter = orbit_position(J2000, 0, std::f64::consts::FRAC_PI_2).unwrap();
        // Independent geometric constraints from Table 1's a/e/I/Omega.
        assert!((length(peri) - 0.38709927 * (1.0 - 0.20563593)).abs() < 1e-12);
        assert!((length(apo) - 0.38709927 * (1.0 + 0.20563593)).abs() < 1e-12);
        let normal = [
            peri[1] * quarter[2] - peri[2] * quarter[1],
            peri[2] * quarter[0] - peri[0] * quarter[2],
            peri[0] * quarter[1] - peri[1] * quarter[0],
        ];
        let inclination = (normal[2] / length(normal)).acos().to_degrees();
        let ascending_node = normal[0].atan2(-normal[1]).to_degrees();
        assert!((inclination - 7.00497902).abs() < 1e-10);
        assert!((ascending_node - 48.33076593).abs() < 1e-10);
    }

    #[test]
    fn rendered_guides_and_moving_planets_share_the_same_ellipse() {
        for date in [
            VALID_JULIAN_DATE_MIN,
            J2000,
            2_461_330.5,
            VALID_JULIAN_DATE_MAX,
        ] {
            let positions = heliocentric_positions(date).unwrap();
            let axes = semi_major_axes(date).unwrap();
            for index in 0..8 {
                let peri = orbit_position(date, index, 0.0).unwrap();
                let apo = orbit_position(date, index, std::f64::consts::PI).unwrap();
                // Sum of distances to the two foci is 2a. One focus is the
                // Sun at zero; the other is peri + apo. This independently
                // checks that the orbit is focus-centred, not centre-centred.
                let second_focus = std::array::from_fn(|i| peri[i] + apo[i]);
                for p in [
                    positions[index],
                    peri,
                    apo,
                    orbit_position(date, index, 0.73).unwrap(),
                    orbit_position(date, index, 4.91).unwrap(),
                ] {
                    let focus_sum = length(p) + length(difference(p, second_focus));
                    assert!((focus_sum - 2.0 * axes[index]).abs() < 1e-11);
                    assert!(p.into_iter().all(f64::is_finite));
                }
            }
        }
    }

    #[test]
    fn kepler_motion_obeys_equal_areas_and_moves_prograde() {
        for e in [0.0, 0.01671123, 0.09339410, 0.20563593] {
            for mean in [-3.1, -0.5, 0.0, 0.8, 3.1] {
                let anomaly = eccentric_anomaly(mean, e).unwrap();
                assert!((anomaly - e * anomaly.sin() - mean).abs() < 1e-12);
            }
        }
        // A positive elapsed second advances each planet's mean anomaly;
        // the prior decorative scene moved several orbits backwards.
        let before = heliocentric_positions(J2000).unwrap();
        let after = heliocentric_positions(J2000 + 1.0 / SECONDS_PER_DAY).unwrap();
        for (a, b) in before.into_iter().zip(after) {
            assert!(a[0] * b[1] - a[1] * b[0] > 0.0);
        }
    }
    #[test]
    fn selected_dates_match_independent_horizons_de441_vectors_within_fit_error() {
        // Retrieved 2026-10-17 from https://ssd.jpl.nasa.gov/api/horizons.api
        // COMMAND=1..8, CENTER='500@10', EPHEM_TYPE='VECTORS', VEC_TABLE=2,
        // REF_PLANE='ECLIPTIC', REF_SYSTEM='ICRF', OUT_UNITS='AU-D',
        // TLIST='2451545.0,2461330.5'. Geometric Sun-centred DE441 vectors.
        // These conservative bounds are regression limits at these two dates,
        // not guaranteed model errors over the whole 1800-2050 interval.
        let references = [
            [
                [
                    -0.1300936053754522,
                    -0.4472876181353563,
                    -0.02459830695805179,
                ],
                [
                    -0.718302296345389,
                    -0.03265430819980606,
                    0.04101418202684621,
                ],
                [
                    -0.1771587841839055,
                    0.9672193524609504,
                    -1.139275508446145e-06,
                ],
                [
                    1.390715921746287,
                    -0.01341631815233992,
                    -0.03446766277585426,
                ],
                [4.001177161126058, 2.938576106966103, -0.101785276633744],
                [6.406408859532809, 6.569989616912557, -0.3690764262448534],
                [14.43185661438178, -13.73432146877096, -0.2381416877992503],
                [16.81204696805288, -24.99176288928469, 0.1272228799202476],
            ],
            [
                [
                    0.2968545867480549,
                    -0.2858736239810546,
                    -0.05058919006843763,
                ],
                [0.6850048256240836, 0.2352798591512265, -0.03629075063270362],
                [
                    0.9157158735726046,
                    0.3937357274244755,
                    -2.956846253613926e-05,
                ],
                [-0.08796145826538036, 1.574637431209227, 0.03515544965670159],
                [-3.581978070412086, 3.922238642290854, 0.06384896232910235],
                [9.237057442613429, 1.854074842791062, -0.399981207212825],
                [8.874238761698079, 17.2943892723211, -0.05083483594656791],
                [29.83478645945119, 1.430581495819291, -0.7169946128059855],
            ],
        ];
        let limits_au = [
            0.00003, 0.00007, 0.00008, 0.00025, 0.009, 0.030, 0.010, 0.009,
        ];
        for (date, reference) in [J2000, 2_461_330.5].into_iter().zip(references) {
            let actual = heliocentric_positions(date).unwrap();
            for index in 0..8 {
                let error = length(difference(actual[index], reference[index]));
                assert!(
                    error <= limits_au[index],
                    "planet {index} at JD {date}: error {error} AU exceeds selected-date regression bound"
                );
            }
        }
    }
}
