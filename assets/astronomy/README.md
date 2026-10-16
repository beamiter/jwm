# Astronomy textures

## Earth (default October surface)

Credit: NASA Earth Observatory / Blue Marble Next Generation, October 2004.
This is a fixed historical monthly surface composite, not live satellite imagery,
current weather, or an automatically changing monthly map. October is selected
for the autumn visual reference; sunlight still follows the actual UTC/date.

- Source: https://science.nasa.gov/earth/earth-observatory/blue-marble-next-generation/base-map/
- Original 5400 × 2700 JPEG: https://assets.science.nasa.gov/content/dam/science/esd/eo/images/bmng/bmng-base/october/world.200410.3x5400x2700.jpg
- File: earth-nasa-october.jpg (1,766,694 bytes, unchanged original)
- SHA256: ccfd9b7a5e5ee601e1466556716bba50ea418f3a8e068ab0194ae0425815b1b0

## Earth (retained January surface)

The prior January image remains bundled and installed for explicit texture paths.
An existing user-owned legacy January override in the data-home directory remains
preferred over the bundled October default when no newer October override exists.


Credit: NASA Earth Observatory / Blue Marble Next Generation, January 2004.
This is a monthly surface composite, not live satellite imagery or weather.

- Source: https://science.nasa.gov/earth/earth-observatory/blue-marble-next-generation/base-map/
- Original 5400 × 2700 JPEG: https://assets.science.nasa.gov/content/dam/science/esd/eo/images/bmng/bmng-base/january/world.200401.3x5400x2700.jpg
- File: earth-nasa-january.jpg (1,884,678 bytes, unchanged original)
- SHA256: 99f5faad74efe985fbf1714c8be7296ca9999759a1215b65f99b7f1df278dde5

## Moon

Credit: NASA's Scientific Visualization Studio. CGI Moon Kit color map (2025),
visualized by Ernie Wright (USRA), from Lunar Reconnaissance Orbiter Camera
(LROC) and Lunar Orbiter Laser Altimeter (LOLA) data.
This is a processed surface color composite for visualization, not live imagery
or a scientific measurement product. Polar gaps use lower-resolution LOLA data.

- Source: https://svs.gsfc.nasa.gov/4720/
- Original 2048 × 1024 JPEG: https://svs.gsfc.nasa.gov/vis/a000000/a004700/a004720/lroc_color_2k.jpg
- File: moon-nasa-lroc.jpg (457,942 bytes, unchanged original)
- SHA256: f7130a1822681fa7512d7dcfd40db8c10b9ba4f06777910348698260ed7a2170

## Earth clouds

Credit: NASA Goddard Space Flight Center / Reto Stöckli (clouds), with
Blue Marble compositing enhancements by Robert Simmon. Terra/MODIS.
This static historical Blue Marble 2002 visualization composite combines two days
of visible imagery and a third day of thermal infrared imagery over the poles.
It is not current weather, a single observation, or a climatological average.

- Source: https://science.nasa.gov/earth/earth-observatory/the-blue-marble-true-color-global-imagery-at-1km-resolution/
- Original 2048 × 1024 JPEG: https://eoimages.gsfc.nasa.gov/images/imagerecords/57000/57747/cloud_combined_2048.jpg
- File: earth-nasa-clouds.jpg (829,367 bytes, unchanged original)
- SHA256: daddaad84d7a33bbbc86cdda3f591099f57cee8607b7bcf3b67eb7e4f7a1c793
- RGB grayscale with identical channels and no alpha. Black denotes clear areas;
  white denotes dense clouds. Luminance is used as a visual opacity mask, not
  calibrated cloud optical depth. Source polar/land residuals may remain.
- North-up, Greenwich-centered map orientation is visually verified from residual
  land outlines; no independent georeferencing metadata is supplied with this JPEG.
- Reuse guidance: https://science.nasa.gov/earth/faq/ (reuse including commercial
  use is permitted except where copyright is indicated; none indicated here).

## Milky Way background

Credit: NASA/Goddard Space Flight Center Scientific Visualization Studio.
Gaia DR2: ESA/Gaia/DPAC. Visualization by Ernie Wright (USRA).

This is NASA SVS's catalogue-derived faint-star map in galactic coordinates,
not a photograph, live sky, nebula gas simulation, or view from the user's location.
It omits the bright Hipparcos/Tycho foreground. Its stellar-density structure
reveals the Milky Way and dark dust lanes. JWM uses a fixed, rotated decorative
crop and reduced display brightness; it is not an astronomical orientation tool.
No constellation overlays, logos, third-party photographs, or AI-generated pixels
are included. No procedural stars are added when this texture is available.

- Source: https://svs.gsfc.nasa.gov/4851/
- Original: https://svs.gsfc.nasa.gov/vis/a000000/a004800/a004851/milkyway_2020_4k_gal.exr
- Source SHA256: fad63b36aa632c691d521cd9d8b828de9d29b4c2784276eec44e54c6cb159a49
- File: milkyway-nasa-gaia.jpg (2048 × 1024, 567,194 bytes)
- File SHA256: 0c9247995a0f31eebe8ba49e3994d3fcc468c4cd4bf9a1555445141d81c8078e
- Derivation: decode the 4096 × 2048 linear EXR using FFmpeg 7.1.5 to planar
  float RGB, downsample each linear channel using Lanczos, clamp to [0,1],
  apply the standard piecewise sRGB transfer, round to 8-bit, and encode JPEG
  at quality 91, 4:4:4. Reproduction tool: scripts/prepare-astronomy-sky.py.
  Source EXR is not bundled. JPEG decoding uses ordinary UNORM sampling;
  the shader does not apply a second gamma transfer to the sky.
- SVS usage: https://svs.gsfc.nasa.gov/help/ identifies its content as public
  domain unless otherwise noted. The source item lists no copyright exception.
- Gaia data credit guidance:
  https://gea.esac.esa.int/archive/documentation/GDR2/Miscellaneous/sec_credit_and_citation_instructions/
  Gaia data are open and free to use with credit to ESA/Gaia/DPAC. This source
  credit is retained here alongside the NASA SVS credit; it is not a claim that
  all ESA imagery is public domain.
- Gaia acknowledgement: This work has made use of data from the European Space
  Agency (ESA) mission Gaia, processed by the Gaia Data Processing and Analysis
  Consortium (DPAC). Funding for the DPAC has been provided by national
  institutions, in particular the institutions participating in the Gaia
  Multilateral Agreement.
- If this optional asset is missing or cannot be decoded, JWM retains its
  original stationary procedural background instead of blocking the wallpaper.

## Usage and installation

- NASA images and media usage guidance: https://www.nasa.gov/nasa-brand-center/images-and-media/

NASA imagery is generally available for informational use under that guidance;
third-party materials have their own restrictions. These NASA sources are credited
separately from JWM's software license. No NASA logo is included. NASA has not
reviewed, endorsed, or sponsored JWM or these independently rendered visualizations.

The repository bundles these images as data files. Source installs copy them with
this credit to /usr/local/share/jwm/astronomy/. Release archives keep the images and credit
inside each version's share/jwm/astronomy/ so rollback restores matching assets.
JWM never downloads these files at runtime and does not embed them in the binary.
