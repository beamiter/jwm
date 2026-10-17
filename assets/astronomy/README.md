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

### Moon close-up detail

The optional 8192 × 4096 detail asset uses the same NASA SVS 2025 LROC/LOLA
colour composite as the startup map, retaining four times its width and height.
Credit and scientific limitations are the same as above: source camera coverage
extends from 70°S to 70°N, with lower-resolution LOLA albedo filling polar gaps.
This remains a processed colour visualization, not metre-scale terrain geometry.

- Source: https://svs.gsfc.nasa.gov/4720/
- Original RGB16 sRGB TIFF: https://svs.gsfc.nasa.gov/vis/a000000/a004700/a004720/lroc_color_16bit_srgb_8k.tif
- Original size: 243,289,792 bytes
- Original SHA256: db7808e878b6a55eb409bb231eab8deb477f84b5c9d7396d76ff73e5d54992d9
- File: moon-nasa-lroc-8k.jpg (8192 × 4096, 8,018,121 bytes)
- File SHA256: 821831cacac483ebb5390e1c6b4039b6274f7d1f9c10b3c35259496af6b0cac0
- Derivation: FFmpeg 6.1.1 decodes native RGB16 sRGB to RGB48LE; each channel is
  quantized using `(value + 128) / 257`, rounded down, then saved by Pillow 12.3.0
  as an RGB8 JPEG at quality 91 with 4:4:4 chroma. No resampling, extra gamma
  transfer, exposure adjustment, sharpening or generated image detail is used.
- Reproduction: `python3 scripts/prepare-astronomy-moon-detail.py SOURCE.tif`
- Runtime: the small original map remains the required startup asset. The detail
  texture is requested lazily only after approaching the Moon, decoded through
  the bounded wallpaper worker, and capped by both 8192 pixels and the GPU's
  supported texture dimension. Failure retains the startup image. No runtime
  download occurs. Optional shader material grain is artistic microcontrast,
  separate from the geographic features supplied by this image.

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
  the Astronomy Earth/Moon shader does not apply a second gamma transfer to the
  sky. Solar System decodes this sRGB sample to linear light before applying its
  scene-wide tone mapper and output transfer, at reduced exposure.
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

## Full-sky star and constellation atlas

Credit: NASA SVS / Goddard; Ernie Wright (USRA). Gaia DR2: ESA/Gaia/DPAC.
The full star map includes the Hipparcos and Tycho foreground along with Gaia's
faint stars. Constellation figures follow Alan MacRobert's patterns developed
for the IAU, with credit to Sky & Telescope, Roger Sinnott and Rick Fienberg.
These figure shapes are conventional illustrations, not official IAU borders.

Both input maps use NASA's corrected galactic projection: longitude zero is at
the centre and positive longitude runs left. This is a distant sky panorama,
not a volume simulation or the current sky above the user's geographic location.
Nebula artwork and text labels are not baked into this data asset.

- Source: https://svs.gsfc.nasa.gov/4851/
- Star source: https://svs.gsfc.nasa.gov/vis/a000000/a004800/a004851/starmap_2020_8k_gal.exr
- Star source size: 160,735,772 bytes (8192 × 4096, linear half-float EXR)
- Star source SHA256: d70924422d3e0159764b16a19658784befcf73b97e06e5c16614860c1514bb58
- Figure source: https://svs.gsfc.nasa.gov/vis/a000000/a004800/a004851/constellation_figures_4k_gal.tif
- Figure source size: 363,912 bytes (4096 × 2048, grayscale TIFF)
- Figure source SHA256: acca4620586513dec05eb22920ac6ba8847a5ce8902a85fb37f386b32aa08757
- File: galaxy-nasa-4k.png (4096 × 2048, 12,359,357 bytes)
- File SHA256: f92ee3d1eff038c93200a79f0a0496edb4bb37cf397de676d7d57c1e85cc8d94
- Derivation: FFmpeg 6.1.1 decodes RGB radiance; Pillow 12.3.0 downsamples each
  linear channel with Lanczos. Values are clamped to [0,1], encoded with the
  standard piecewise sRGB transfer, and rounded to RGB8. The PNG's fourth channel
  is `255 - figure_intensity`, copied from the native grayscale figure map.
  No premultiplication, exposure adjustment, generated imagery or text raster is
  applied. The exported PNG is verified against every source-derived RGBA byte.
- **Alpha is data, not opacity.** The shader reads `1 - alpha` as line intensity.
  RGB beneath zero alpha must be preserved. An ordinary opaque fallback image
  supplies alpha 1 and therefore contributes no constellation overlay.
- Reproduction: `python3 scripts/prepare-astronomy-galaxy.py STARS.exr FIGURES.tif`
- Runtime decoding preserves straight RGBA through the shared bounded worker.
  It retains the native 4K size where supported and downsamples to the observed
  GPU limit when needed. The prior faint-star JPEG remains a compatibility asset.
  NASA SVS usage and Gaia acknowledgement above also apply to this atlas.

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
