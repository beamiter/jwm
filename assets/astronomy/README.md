# Astronomy textures

## Earth

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
