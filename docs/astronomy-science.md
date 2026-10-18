# Celestial science, sources and display limits

JWM's celestial wallpapers combine source-backed reference facts, historical
spacecraft maps, approximate date-derived planetary positions and interactive
3D presentation. The default scientific presentation uses observed maps where
they are available and restrained materials where they are not. Its purpose is
to make the underlying astronomy understandable while keeping the boundaries
of the data visible.

**Solar System is an approximate, distance-compressed model.** Its planets are
enlarged, its camera is freely movable, and its images are historical composites.
The single-body gallery is an interactive presentation of a body, not a
spacecraft reconstruction at a calibrated position and epoch. These distinctions
apply even when `wallpaper_astronomy_presentation = "scientific"` is selected.

For gestures and output configuration, see
[Celestial explorer](celestial-explorer.md) and
[Astronomy wallpaper](astronomy-wallpaper.md). Exact image bytes, credits,
derivations and checksums are in the
[astronomy asset attribution](../assets/astronomy/README.md).

## What the two presentation settings mean

The material setting and the orbital model are independent:

| Setting | Scientific/date-derived option | Illustration option |
| --- | --- | --- |
| `wallpaper_astronomy_presentation` | `"scientific"`: observed maps with analytic optical display layers and subdued fallback materials; no generated terrain, named storms or procedural nebula artwork | `"illustrative"`: optional artistic material detail, nebulae and enhanced lighting |
| `wallpaper_solar_orbit_mode` | `"ephemeris"`: offline JPL fitted Keplerian positions for the selected date | `"illustrative"`: the legacy animated orbital tour |

Both presentations retain finite map resolution and enlarged display bodies.
Changing appearance does not turn a processed mosaic into a calibrated albedo,
elevation, atmospheric, or live-observation product. If an image is unavailable,
scientific mode uses an explicitly simplified material rather than assigning
another world's geography to that body. Without a sky atlas it uses a plain
background. User-provided images retain their own provenance; the catalogue's
image credits describe the installed defaults.

## Reference facts in the catalogue

`celestial_catalog.rs` supplies stable identifiers, English and Chinese names,
mean radius, sidereal rotation and orbital periods, obliquity, a short
description, material status and direct source links. The catalogue order is
Sun, Mercury, Venus, Earth, Mars, Jupiter, Saturn, Uranus, Neptune, Moon. Galaxy
is a sky scene and does not acquire an invented planetary radius or period.

Planetary radii are **volumetric mean radii** from
[JPL's planetary physical parameters](https://ssd.jpl.nasa.gov/planets/phys_par.html).
They are distinct from equatorial radii, particularly for the giant planets.
Solar System uses oblate navigation geometry for the four giant planets, with
separate NASA polar/equatorial radius ratios; other displayed bodies remain
spherical. Screen size is not a physical radius measurement. Rotation, orbit
and obliquity values link to the
individual [NASA planetary fact sheets](https://nssdc.gsfc.nasa.gov/planetary/factsheet/).
These are conventional reference quantities, not constantly refreshed
measurements.

Several definitions matter when comparing bodies:

- A sidereal rotation is measured relative to the distant reference frame.
  Earth's approximately 23.9345-hour rotation differs from its 24-hour solar day.
  The Moon's approximately 27.3217-day sidereal orbit differs from its roughly
  29.5-day phase cycle. The lunar orbital period describes an orbit about Earth;
  planetary orbital periods describe orbits about the Sun.
- The catalogue stores Venus and Uranus with signed retrograde rotation periods
  and full, greater-than-90° obliquities. Rendering must apply this reversal
  once: a flipped angular-momentum axis uses the magnitude of the spin period.
- The Sun's adopted 609.12-hour reference rotation is measured at 16° latitude.
  Solar differential rotation is not represented by a single rigid period.
  Gas-giant reference rotation systems likewise do not make changing cloud
  bands and storms rigid surface markings.
- The Moon's listed 6.68° obliquity is relative to its own orbit; it is not its
  approximately 1.54° inclination to the ecliptic. A tilt angle alone does not
  specify a pole's direction on the celestial sphere.

The specific reference pages for these distinctions are
[Earth](https://nssdc.gsfc.nasa.gov/planetary/factsheet/earthfact.html),
[Moon](https://nssdc.gsfc.nasa.gov/planetary/factsheet/moonfact.html),
[Sun](https://nssdc.gsfc.nasa.gov/planetary/factsheet/sunfact.html),
[Venus](https://nssdc.gsfc.nasa.gov/planetary/factsheet/venusfact.html),
[Jupiter](https://nssdc.gsfc.nasa.gov/planetary/factsheet/jupiterfact.html),
[Saturn](https://nssdc.gsfc.nasa.gov/planetary/factsheet/saturnfact.html) and
[Uranus](https://nssdc.gsfc.nasa.gov/planetary/factsheet/uranusfact.html).

## Solar System positions and time

The offline model implements Table 1 from
[JPL's Approximate Positions of the Planets](https://ssd.jpl.nasa.gov/planets/approx_pos.html).
Six fitted orbital elements and their secular rates determine each body's
ellipse. A double-precision Kepler solve determines its position, including
faster motion near perihelion. The Sun occupies the origin. This is a fitted
low-accuracy model, not an integrated N-body calculation or a bundled DE/SPICE
ephemeris. The third output represents the **Earth–Moon barycentre**; the Solar
System Earth display is placed there rather than at an independently calculated
geocentre. The Moon remains a separate gallery scene, not a propagated lunar
orbit in this eight-planet model.

The direct numerical API accepts Julian dates in TDB from 1800-01-01 to
2050-01-01, including the endpoints. It rejects non-finite values and dates
outside the fitted interval instead of extrapolating. The runtime UTC playback
window is narrower: **1972-01-01 00:00:00 through 2049-12-31 23:58:00 UTC**.
The upper margin keeps the dynamical date within the fit after the UTC-to-TDB
conversion. At either boundary the displayed date stops advancing; an
out-of-range playback does not silently become a fictional orbit.

The UTC conversion follows the leap-second offsets and short periodic
TT-to-TDB approximation documented in
[NAIF's public leap-second kernel](https://naif.jpl.nasa.gov/pub/naif/generic_kernels/lsk/naif0012.tls).
The embedded table covers integer leap-second transitions from 1972 through
the 2017 offset of 37 seconds. Before 1972, UTC requires a different historical
model, so the POSIX conversion rejects those instants. Future leap seconds
require updating the table. POSIX timestamps do not represent the leap second
itself. No local-time-zone or daylight-saving offset is added to astronomical
time.

### Orbital playback and surface rotation are separate

`wallpaper_solar_time_scale` controls date-derived orbital playback. Its range
is 0 to 1,000,000: zero holds the orbital date, one follows current UTC, and
86,400 advances one simulated day per real second. The offset setting,
`wallpaper_solar_time_offset_days`, reanchors the timeline relative to current
UTC. Rate changes integrate the previous rate before switching, so pause and
resume preserve the displayed date. Selecting real time or changing the offset
explicitly reanchors it.

Solar surface rotation has a separate `wallpaper_solar_spin_speed` multiplier.
The default 718.035 makes Earth's sidereal rotation take 120 seconds; **it is
accelerated even when orbital time is 1×**. Set it to one for reference-period
rotation, or zero to pause it. Changing this rate preserves the current surface
phase. Its elapsed-time phase is not an IAU prime-meridian solution at the
selected calendar date. General single-body tours also remain presentation
controls. Earth's day/night calculation and the Moon's phase in the independent
Astronomy gallery retain their actual UTC clock; the Solar orbital timeline
does not retime them.

### Coordinate systems and the compressed scene

The numerical model returns heliocentric astronomical units in the mean
ecliptic and equinox of J2000. One astronomical unit is exactly
149,597,870.7 km under the
[IAU definition recorded by JPL](https://ssd.jpl.nasa.gov/astro_par.html).
The renderer uses the right-handed mapping
`[x, y, z] → [x, z, -y]`: its positive Y direction is ecliptic north.

For planet `i`, the entire orbit is uniformly rescaled by
`display_semimajor_axis[i] / astronomical_semimajor_axis[i]`. This preserves
that ellipse's eccentricity, orientation, inclination and heliocentric direction.
Each orbit has its own factor, so distances between planets and views from an
off-centre camera do not preserve a common physical scale. Planetary radii are
enlarged separately. Apparent separations, occlusions and transit timings in
this navigable scene are consequently display effects.

The CPU computes each planet centre and the corresponding ellipse's centre
and two axes from the same date and elements. The shader's guides and mouse
picking use this shared geometry. A planet therefore remains on its visible
inclined ellipse, and clicking it follows the same position used to draw it.

A close-up isolates the selected body and its own rings, hiding other bodies
and orbital guides so an enlarged neighbour cannot obscure the inspection.
Picking uses the same isolation rule. Sunlight still comes from the scene's
solar direction, and returning to the overview restores the complete system.

In date-derived Solar mode, background rays are rotated from J2000 ecliptic
coordinates by JPL's adopted 23.43928° obliquity into equatorial coordinates,
then into the standard Hipparcos/ICRS Galactic frame. The Galactic convention
is documented by the official
[IAU SOFA manual, `iauIcrs2g`](https://www.iausofa.org/s/manual_c.pdf).
The sky shader then samples NASA's corrected Galactic atlas with longitude
zero at the centre and increasing longitude to the left. This aligns the
reference directions of the orbital model and the atlas; it does not turn
the compressed camera into an observer at a real location.

The independent body/galaxy gallery keeps its presentation frame. Its camera,
lighting and body poles are not registered together to a common IAU observer
epoch. Star proper motion, parallax, atmospheric refraction, aberration,
light-time corrections and observer-location horizon coordinates are not
computed by these wallpapers.

## Accuracy checked against independent ephemerides

The regression suite includes independently requested JPL Horizons DE441
heliocentric vectors at JD TDB 2451545.0 (J2000) and 2461330.5
(2026-10-17 00:00 TDB). These checks can catch coefficient transcription,
rotation-sign, unit and time-scale mistakes. They also expose the finite
accuracy of the fit instead of testing only against copies of its own equations.

The following are the measured Euclidean position differences in the
uncompressed model, rounded to the nearest kilometre. Angular differences
are the angle between heliocentric position vectors; they are not ground-based
observing errors.

| System barycentre | J2000 position difference (km) | 2026-10-17 difference (km) | Largest angular difference across these two samples (arcsec) |
| --- | ---: | ---: | ---: |
| Mercury | 1,030 | 2,865 | 8.46 |
| Venus | 8,112 | 1,188 | 14.80 |
| Earth–Moon | 2,006 | 7,925 | 10.76 |
| Mars | 8,193 | 25,991 | 18.86 |
| Jupiter | 1,149,745 | 461,899 | 309.03 |
| Saturn | 3,848,253 | 1,817,169 | 516.84 |
| Uranus | 1,077,769 | 805,680 | 70.55 |
| Neptune | 1,099,195 | 697,446 | 45.03 |

These selected-date results and the test tolerances are not a guaranteed error
envelope over 250 years. JPL publishes nominal errors for the approximation;
they are not strict maxima against every later integrated ephemeris. Numerical
Kepler convergence does not eliminate model error. High-precision applications
should obtain an appropriate integrated solution through
[JPL Horizons](https://ssd.jpl.nasa.gov/planets/orbits.html).

The reference request uses the
[Horizons API](https://ssd-api.jpl.nasa.gov/doc/horizons.html):

```text
https://ssd.jpl.nasa.gov/api/horizons.api
format=text
COMMAND='1' through '8'         # planetary system barycentres
CENTER='500@10'                 # heliocentric
MAKE_EPHEM='YES'
EPHEM_TYPE='VECTORS'
TLIST='2451545.0,2461330.5'      # Julian dates in TDB
REF_PLANE='ECLIPTIC'
REF_SYSTEM='ICRF'
OUT_UNITS='AU-D'
VEC_TABLE='2'
CSV_FORMAT='YES'
```

Tests additionally check independent geometric constraints: Mercury's
perihelion/aphelion distances and orbit-plane orientation, equal-area Kepler
motion, points lying on their corresponding ellipses, prograde motion in the
J2000 frame, leap-second transitions, invalid input rejection and timeline
clamping. They do not claim a terrain, weather or photometric validation.

## What each visible map represents

| Body or scene | Installed scientific source | Interpretation and remaining limits |
| --- | --- | --- |
| Mercury | [MESSENGER PIA16298](https://science.nasa.gov/photojournal/a-world-view/) | Monochrome observed surface mosaic; inherited illumination, seams and small polar no-data regions; not calibrated albedo or true colour |
| Earth | [Blue Marble Next Generation](https://science.nasa.gov/earth/earth-observatory/blue-marble-next-generation/base-map/) | October 2004 historical surface composite; the cloud layer is a separate historical composite, not current weather |
| Moon | [NASA SVS CGI Moon Kit](https://svs.gsfc.nasa.gov/4720/) | Processed 2025 LROC/LOLA colour visualization; lower-resolution polar coverage; no elevation mesh |
| Mars | [NASA Viking/USGS texture](https://science.nasa.gov/3d-resources/mars/) | Historical processed global surface mosaic, retained at native resolution; colours and added lighting are not calibrated radiance |
| Jupiter | [Cassini PIA07782](https://www.jpl.nasa.gov/images/pia07782-cassinis-best-maps-of-jupiter-cylindrical-map/) | Clouds observed in December 2000; a near-visible reconstruction from two filters; historical storm positions |
| Sky | [NASA SVS 2020 star maps](https://svs.gsfc.nasa.gov/4851/) | Gaia DR2 faint stars plus Hipparcos/Tycho foreground; a distant catalogue panorama, not a navigable stellar volume |
| Sun, Venus, Saturn, Uranus, Neptune | Explicitly simplified material | No globally measured default cloud/surface map is supplied; analytic optical layers and broad cloud reflectance are display approximations, not observed activity or terrain |

The new Mercury and Mars assets retain their original JPEG bytes. Jupiter's
original 3601 × 1801 latitude/longitude grid is resampled once over its full
domain to 3600 × 1800 for the exact 2:1 decoder contract. It is not enlarged,
cropped, flipped or supplied with synthetic detail. The attribution file
records both source and derived checksums and a reproducible conversion command.
Native map meridian order is preserved, but exact epoch-dependent surface
longitude registration has not been independently established for these JPEGs.

NASA's hosted [Saturn 3D texture](https://science.nasa.gov/3d-resources/saturn/)
is identified by its source as a Don Davis fictional map. It was deliberately
excluded from observed materials. An image hosted by a scientific institution
still needs its own provenance check. Likewise, Venus radar surface imagery
would require a labelled radar mode; it cannot serve as visible-light cloud
colour through an opaque atmosphere.

## Visible-light geometry and lighting

The Solar System's giant planets are oblate spheroids using the following
[NASA NSSDCA reference dimensions](https://nssdc.gsfc.nasa.gov/planetary/factsheet/).
Their equatorial radii are defined at the 1 bar atmospheric level; these are
reference atmospheric shapes, not solid surfaces. The polar/equatorial ratio
is calculated from the listed radii, rather than exaggerating the flattening.

| Body | Equatorial radius (km) | Polar radius (km) | Polar/equatorial ratio |
| --- | ---: | ---: | ---: |
| [Jupiter](https://nssdc.gsfc.nasa.gov/planetary/factsheet/jupiterfact.html) | 71,492 | 66,854 | 0.9351256 |
| [Saturn](https://nssdc.gsfc.nasa.gov/planetary/factsheet/saturnfact.html) | 60,268 | 54,364 | 0.9020376 |
| [Uranus](https://nssdc.gsfc.nasa.gov/planetary/factsheet/uranusfact.html) | 25,559 | 24,973 | 0.9770727 |
| [Neptune](https://nssdc.gsfc.nasa.gov/planetary/factsheet/neptunefact.html) | 24,764 | 24,341 | 0.9829188 |

GPU intersections, illumination normals and CPU picking share the tilted
spheroid dimensions. Mapped planetocentric latitude is derived from the
position relative to the centre, independently of the geometric normal, so
flattening does not shift the Cassini cloud map's latitude bands. Saturn also
casts its oblate shadow onto its rings. Display radii and orbital distances
still have separate scale factors; this shape refinement does not make the
whole scene a common-scale physical model.

The near-white solar photosphere uses limb darkening and pixel-filtered
analytic granulation, following the qualitative features described by
[NASA's solar-physics reference](https://solarscience.msfc.nasa.gov/surface.shtml).
Granules are generic display structure, not observed solar pixels; no current
sunspots or activity regions are generated. The pale outer glow has a separate
enhanced exposure. In visible light the real corona is normally overwhelmed by
the photosphere, as described in [NASA's Sun facts](https://science.nasa.gov/sun/facts/).
The glow therefore does not represent calibrated coronal radiance or a plasma
and magnetic-field simulation.

Saturn's flat rings use C/B/A regions based on
[NASA's ring dimensions and optical-depth ranges](https://nssdc.gsfc.nasa.gov/planetary/factsheet/satringfact.html).
The radial density profile and fine ringlets are analytic approximations with
pixel-footprint filtering. Beer-Lambert transmission depends on viewing angle;
sun-facing reflection and opposite-side transmission use distinct phase
responses. Planet-on-ring and ring-on-planet shadows share this geometry and
optical depth. These reproduce the kinds of translucent rings and shadows in
[Cassini imagery](https://science.nasa.gov/photojournal/rings-and-shadows/),
without reconstructing individual particles or measured ring photometry.

Earth's historical cloud image is placed on a separate shell with approximate
shadows. A blue-colour mask inferred from the surface image limits GGX ocean
reflection; neither the mask nor its roughness is a measured surface product.
The nearest visible atmosphere uses at most six integration samples with
altitude-dependent density and solar occlusion. Its colours, density and
scattering weights are chosen for display, not fitted spectral radiance,
multiple scattering or current weather. These layers run in Scientific
presentation as well; installed observed maps continue to take priority over
fallback materials and generated geographic detail remains excluded.

Image inputs are decoded once and the layers are combined in linear light,
followed by one filmic tone map and display transfer. Solar, planetary and sky
exposure factors are adjusted separately for legibility. This is not the
dynamic range of a single camera exposure: NASA's
[Cassini exposure example](https://science.nasa.gov/photojournal/not-really-starless-at-saturn/)
explains why most stars disappear when bright rings are correctly exposed.
No new textures or runtime downloads are needed for these optical layers.
Their reference sources were checked on 2026-10-18.

## Rendering fidelity that remains a presentation choice

Reference spheroids, surface reflection, tone mapping, atmospheric layers, the
solar luminous surface and Saturn's ring optical depth remain approximations.
Resolved relief, spectral multiple scattering, thermal emission and changing
weather are not reconstructed. A finite mosaic cannot reveal unlimited real
detail as the camera approaches.

In date-derived Solar mode, each planetary spin axis preserves the catalogue's
obliquity relative to its actual fitted orbit plane. The direction of the tilt
within that plane is chosen reproducibly by projecting display +X into the
plane. This fixes an earlier mismatch with the legacy illustrative plane,
but it does not calibrate the pole azimuth to an IAU orientation solution.
General planetary prime-meridian phases also remain uncalibrated.
Historical cloud features are rotated as a texture;
their longitude at a selected date is not a current observation. Gallery
dragging is a camera/presentation action, not a change to the body's physical
rotation or the time driving Earth and Moon illumination.

The catalogue atlas provides a full surrounding sky with recognizable stellar
patterns. Conventional constellation stick figures are not official IAU
constellation boundaries. Illustration mode's coloured nebula shapes are
artwork, even where their reference directions are sourced; science mode
omits that artwork. Neither mode reconstructs the Milky Way as a measured
three-dimensional galaxy or predicts the changing sky above a user's horizon.

Sources, image provenance and regression references were checked on
2026-10-17. Any future replacement map, expanded epoch interval or stronger
accuracy claim should update its source record and independent validation
together with the implementation.
