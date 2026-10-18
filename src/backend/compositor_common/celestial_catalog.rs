//! Source-backed reference facts for the celestial wallpaper picker.
//!
//! Values describe the astronomical bodies, not the deliberately enlarged
//! spheres used to make the wallpaper navigable. In particular, `radius_km`
//! is the volumetric mean radius, not the equatorial radius. Planetary radii
//! come from JPL SSD; rotation/orbital periods and obliquities come from each
//! linked NASA fact sheet. These are conventional reference values rather
//! than instantaneous measurements. The gas giants' reference rotations do
//! not imply that their visible clouds rotate as a rigid solid.
//!
//! Sources checked 2026-10-17. Material descriptions must remain separate
//! from physical facts: a plausible procedural surface is not measured data.

pub(crate) const PLANETARY_PHYSICAL_SOURCE: &str = "https://ssd.jpl.nasa.gov/planets/phys_par.html";

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CelestialBody {
    /// Stable configuration ID; catalogue order also matches shader body IDs.
    pub id: &'static str,
    pub name: &'static str,
    pub name_zh: &'static str,
    /// Radius of a sphere with the same volume, in kilometres.
    pub radius_km: f64,
    /// Sidereal hours; negative values mean retrograde rotation. A renderer
    /// that already flips the spin axis for obliquity > 90 degrees must use
    /// the magnitude here, so that retrograde is not applied twice.
    pub rotation_hours: f64,
    /// Sidereal orbit about `orbit_parent`. The Sun has no heliocentric orbit.
    pub orbital_period_days: Option<f64>,
    pub orbit_parent: Option<&'static str>,
    /// Right-hand-rule obliquity to the body's orbit, in degrees. For the Sun
    /// the reference is the ecliptic; for the Moon it is the lunar orbit,
    /// not the ecliptic. This alone does not fix the pole's celestial azimuth.
    pub obliquity_degrees: f64,
    pub description: &'static str,
    pub source_url: &'static str,
    pub physical_source_url: &'static str,
    /// Describes the installed default material, never a user's custom map.
    pub material_label: &'static str,
    pub material_note: &'static str,
    pub texture_source_url: Option<&'static str>,
}

/// Sun, eight planets, Moon. Keep IDs stable for persisted output assignments.
pub(crate) const BODIES: [CelestialBody; 10] = [
    CelestialBody {
        id: "sun",
        name: "Sun",
        name_zh: "太阳",
        radius_km: 695_700.0,
        rotation_hours: 609.12,
        orbital_period_days: None,
        orbit_parent: None,
        obliquity_degrees: 7.25,
        description: "Our star. Its reference rotation is adopted at 16 degrees latitude; the Sun rotates differentially.",
        source_url: "https://nssdc.gsfc.nasa.gov/planetary/factsheet/sunfact.html",
        physical_source_url: "https://nssdc.gsfc.nasa.gov/planetary/factsheet/sunfact.html",
        material_label: "Photosphere illustration",
        material_note: "A restrained luminous sphere in science mode; optional illustrated granulation and corona are not live solar observations or active-region positions.",
        texture_source_url: None,
    },
    CelestialBody {
        id: "mercury",
        name: "Mercury",
        name_zh: "水星",
        radius_km: 2_439.4,
        rotation_hours: 1_407.6,
        orbital_period_days: Some(87.969),
        orbit_parent: Some("sun"),
        obliquity_degrees: 0.034,
        description: "The innermost planet. Its slow spin and eccentric orbit make a solar day different from its sidereal rotation period.",
        source_url: "https://nssdc.gsfc.nasa.gov/planetary/factsheet/mercuryfact.html",
        physical_source_url: PLANETARY_PHYSICAL_SOURCE,
        material_label: "MESSENGER monochrome mosaic",
        material_note: "NASA PIA16298 is an observed historical surface mosaic with inherited lighting, seams and small polar gaps. It is not a true-colour or calibrated albedo map.",
        texture_source_url: Some("https://science.nasa.gov/photojournal/a-world-view/"),
    },
    CelestialBody {
        id: "venus",
        name: "Venus",
        name_zh: "金星",
        radius_km: 6_051.8,
        rotation_hours: -5_832.6,
        orbital_period_days: Some(224.701),
        orbit_parent: Some("sun"),
        obliquity_degrees: 177.36,
        description: "A slowly rotating retrograde planet. In visible light its opaque clouds hide the solid surface.",
        source_url: "https://nssdc.gsfc.nasa.gov/planetary/factsheet/venusfact.html",
        physical_source_url: PLANETARY_PHYSICAL_SOURCE,
        material_label: "Visible-cloud illustration",
        material_note: "The pale cloud deck is illustrative. Radar maps of the surface would be a separate observation mode, not visible-light colour.",
        texture_source_url: None,
    },
    CelestialBody {
        id: "earth",
        name: "Earth",
        name_zh: "地球",
        radius_km: 6_371.0084,
        rotation_hours: 23.9345,
        orbital_period_days: Some(365.256),
        orbit_parent: Some("sun"),
        obliquity_degrees: 23.44,
        description: "Our home planet. A sidereal rotation is slightly shorter than a 24-hour solar day; sunlight follows physical time independently of camera motion.",
        source_url: "https://nssdc.gsfc.nasa.gov/planetary/factsheet/earthfact.html",
        physical_source_url: PLANETARY_PHYSICAL_SOURCE,
        material_label: "NASA surface composite",
        material_note: "Blue Marble October 2004 surface and a historical cloud composite. Neither is live imagery or current weather.",
        texture_source_url: Some(
            "https://science.nasa.gov/earth/earth-observatory/blue-marble-next-generation/base-map/",
        ),
    },
    CelestialBody {
        id: "mars",
        name: "Mars",
        name_zh: "火星",
        radius_km: 3_389.5,
        rotation_hours: 24.6229,
        orbital_period_days: Some(686.980),
        orbit_parent: Some("sun"),
        obliquity_degrees: 25.19,
        description: "A rocky planet with a thin atmosphere. Its sidereal rotation is about 24.6 hours and its year is nearly twice Earth's.",
        source_url: "https://nssdc.gsfc.nasa.gov/planetary/factsheet/marsfact.html",
        physical_source_url: PLANETARY_PHYSICAL_SOURCE,
        material_label: "NASA Viking surface mosaic",
        material_note: "The historical USGS-processed Viking colour mosaic supplies real mapped features. Its display colours are processed; it is not current dust, weather or terrain geometry.",
        texture_source_url: Some("https://science.nasa.gov/3d-resources/mars/"),
    },
    CelestialBody {
        id: "jupiter",
        name: "Jupiter",
        name_zh: "木星",
        radius_km: 69_911.0,
        rotation_hours: 9.9250,
        orbital_period_days: Some(4_332.589),
        orbit_parent: Some("sun"),
        obliquity_degrees: 3.13,
        description: "The largest planet. Its System III reference rotation is distinct from the changing motion of cloud bands and storms.",
        source_url: "https://nssdc.gsfc.nasa.gov/planetary/factsheet/jupiterfact.html",
        physical_source_url: PLANETARY_PHYSICAL_SOURCE,
        material_label: "Cassini 2000 cloud mosaic",
        material_note: "NASA PIA07782 maps clouds observed on 11–12 December 2000, with near-visible colour reconstruction and a small grid resampling. Storms and their longitudes are historical, not current weather.",
        texture_source_url: Some(
            "https://www.jpl.nasa.gov/images/pia07782-cassinis-best-maps-of-jupiter-cylindrical-map/",
        ),
    },
    CelestialBody {
        id: "saturn",
        name: "Saturn",
        name_zh: "土星",
        radius_km: 58_232.0,
        rotation_hours: 10.656,
        orbital_period_days: Some(10_755.699),
        orbit_parent: Some("sun"),
        obliquity_degrees: 26.73,
        description: "A gas giant surrounded by rings. The listed rotation follows conventional System III coordinates, not a rigid cloud layer.",
        source_url: "https://nssdc.gsfc.nasa.gov/planetary/factsheet/saturnfact.html",
        physical_source_url: PLANETARY_PHYSICAL_SOURCE,
        material_label: "Cloud and ring illustration",
        material_note: "Ring shading and optical depth are illustrative; no current cloud map is supplied. The fictional Don Davis texture in NASA's 3D collection is deliberately not bundled as observed data.",
        texture_source_url: None,
    },
    CelestialBody {
        id: "uranus",
        name: "Uranus",
        name_zh: "天王星",
        radius_km: 25_362.0,
        rotation_hours: -17.24,
        orbital_period_days: Some(30_685.400),
        orbit_parent: Some("sun"),
        obliquity_degrees: 97.77,
        description: "An ice giant whose spin axis lies close to its orbital plane. Its reference rotation is retrograde.",
        source_url: "https://nssdc.gsfc.nasa.gov/planetary/factsheet/uranusfact.html",
        physical_source_url: PLANETARY_PHYSICAL_SOURCE,
        material_label: "Atmosphere illustration",
        material_note: "A restrained visible-colour approximation, not a global measured cloud map or an infrared image.",
        texture_source_url: None,
    },
    CelestialBody {
        id: "neptune",
        name: "Neptune",
        name_zh: "海王星",
        radius_km: 24_622.0,
        rotation_hours: 16.11,
        orbital_period_days: Some(60_189.018),
        orbit_parent: Some("sun"),
        obliquity_degrees: 28.32,
        description: "The outermost of the eight planets. Its atmosphere changes over time; a historical image cannot specify today's storms.",
        source_url: "https://nssdc.gsfc.nasa.gov/planetary/factsheet/neptunefact.html",
        physical_source_url: PLANETARY_PHYSICAL_SOURCE,
        material_label: "Atmosphere illustration",
        material_note: "A visible-colour approximation. Strong blue contrast and fixed storm patterns must not be presented as calibrated current observations.",
        texture_source_url: None,
    },
    CelestialBody {
        id: "moon",
        name: "Moon",
        name_zh: "月球",
        radius_km: 1_737.4,
        rotation_hours: 655.720,
        orbital_period_days: Some(27.3217),
        orbit_parent: Some("earth"),
        obliquity_degrees: 6.68,
        description: "Earth's natural satellite. The 27.3-day sidereal orbit differs from the roughly 29.5-day cycle of lunar phases.",
        source_url: "https://nssdc.gsfc.nasa.gov/planetary/factsheet/moonfact.html",
        physical_source_url: "https://nssdc.gsfc.nasa.gov/planetary/factsheet/moonfact.html",
        material_label: "NASA LROC/LOLA composite",
        material_note: "NASA's 2025 CGI Moon Kit uses measured LROC colour with lower-resolution polar albedo and processing for visualisation; it is not a terrain mesh.",
        texture_source_url: Some("https://svs.gsfc.nasa.gov/4720/"),
    },
];

pub(crate) fn body_by_id(id: &str) -> Option<&'static CelestialBody> {
    BODIES.iter().find(|body| body.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_ids_preserve_shader_and_saved_output_contract() {
        let expected = [
            "sun", "mercury", "venus", "earth", "mars", "jupiter", "saturn", "uranus", "neptune",
            "moon",
        ];
        assert_eq!(BODIES.map(|body| body.id), expected);
        for (index, id) in expected.iter().enumerate() {
            assert_eq!(body_by_id(id), Some(&BODIES[index]));
        }
        assert!(body_by_id("pluto").is_none());
        assert!(body_by_id("Earth").is_none());
    }

    #[test]
    fn reference_quantities_keep_their_scientific_meaning() {
        let earth = body_by_id("earth").unwrap();
        let moon = body_by_id("moon").unwrap();
        assert!(earth.rotation_hours < 24.0);
        assert!((moon.rotation_hours / 24.0 - moon.orbital_period_days.unwrap()).abs() < 0.001);
        assert_eq!(moon.orbit_parent, Some("earth"));
        assert!(body_by_id("sun").unwrap().orbital_period_days.is_none());
        assert!(body_by_id("jupiter").unwrap().radius_km > earth.radius_km * 10.0);
        for body in &BODIES {
            assert!(body.radius_km.is_finite() && body.radius_km > 0.0);
            assert!(body.rotation_hours.is_finite() && body.rotation_hours != 0.0);
            assert!((0.0..=180.0).contains(&body.obliquity_degrees));
            if let Some(period) = body.orbital_period_days {
                assert!(period.is_finite() && period > 0.0);
            }
            if ["venus", "uranus"].contains(&body.id) {
                assert!(body.rotation_hours < 0.0 && body.obliquity_degrees > 90.0);
            }
            assert!(body.source_url.starts_with("https://"));
            assert!(!body.material_label.is_empty() && !body.material_note.is_empty());
        }
    }
}
