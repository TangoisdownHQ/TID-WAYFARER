-- =====================================================================
-- 🌌 TIDasONE — Seed: bodies, host stars, exoplanets, surface features
-- ID conventions:
--   0–999          NAIF major bodies (Sun, planets, moons)
--   2_000_001+     NAIF asteroids
--   100_001+       host stars (synthetic — no NAIF id)
--   200_001+       exoplanets (synthetic)
-- Idempotent: every INSERT uses ON CONFLICT DO NOTHING.
-- =====================================================================

-- ---------------------------------------------------------------------
-- Solar System — Sun, planets, major moons, Pluto/Charon
-- ---------------------------------------------------------------------
INSERT INTO bodies (body_id, name, parent_id, body_class, frame, radius_km, gravity_ms2, rotation_hr, ephemeris_src) VALUES
  (10,  'Sun',       NULL, 'star',         'IAU_SUN',       695700.0, 274.0,  609.12, 'naif'),

  (199, 'Mercury',   10,   'planet',       'IAU_MERCURY',     2439.7,   3.7,   1407.6, 'naif'),
  (299, 'Venus',     10,   'planet',       'IAU_VENUS',       6051.8,   8.87, -5832.4, 'naif'),

  (399, 'Earth',     10,   'planet',       'IAU_EARTH',       6371.0,   9.807,   23.9345, 'naif'),
  (301, 'Moon',      399,  'moon',         'IAU_MOON',        1737.4,   1.625,  655.72,  'naif'),

  (499, 'Mars',      10,   'planet',       'IAU_MARS',        3389.5,   3.711,   24.6229, 'naif'),
  (401, 'Phobos',    499,  'moon',         'IAU_PHOBOS',        11.27,  0.0057,   7.65,   'naif'),
  (402, 'Deimos',    499,  'moon',         'IAU_DEIMOS',         6.2,   0.003,   30.3,    'naif'),

  (599, 'Jupiter',   10,   'planet',       'IAU_JUPITER',    69911.0,  24.79,    9.9,    'naif'),
  (501, 'Io',        599,  'moon',         'IAU_IO',          1821.6,   1.796,   42.46,  'naif'),
  (502, 'Europa',    599,  'moon',         'IAU_EUROPA',      1560.8,   1.314,   85.23,  'naif'),
  (503, 'Ganymede',  599,  'moon',         'IAU_GANYMEDE',    2634.1,   1.428,  171.71,  'naif'),
  (504, 'Callisto',  599,  'moon',         'IAU_CALLISTO',    2410.3,   1.235,  400.54,  'naif'),

  (699, 'Saturn',    10,   'planet',       'IAU_SATURN',     58232.0,  10.44,   10.7,    'naif'),
  (601, 'Mimas',     699,  'moon',         'IAU_MIMAS',        198.2,   0.064,   22.62,  'naif'),
  (602, 'Enceladus', 699,  'moon',         'IAU_ENCELADUS',    252.1,   0.113,   32.89,  'naif'),
  (605, 'Rhea',      699,  'moon',         'IAU_RHEA',         763.8,   0.264,  108.42,  'naif'),
  (606, 'Titan',     699,  'moon',         'IAU_TITAN',       2574.7,   1.352,  382.69,  'naif'),
  (608, 'Iapetus',   699,  'moon',         'IAU_IAPETUS',      734.5,   0.223, 1903.93,  'naif'),

  (799, 'Uranus',    10,   'planet',       'IAU_URANUS',     25362.0,   8.69,  -17.24,   'naif'),
  (703, 'Titania',   799,  'moon',         'IAU_TITANIA',      788.4,   0.379,  208.94,  'naif'),
  (704, 'Oberon',    799,  'moon',         'IAU_OBERON',       761.4,   0.346,  323.12,  'naif'),

  (899, 'Neptune',   10,   'planet',       'IAU_NEPTUNE',    24622.0,  11.15,   16.11,   'naif'),
  (801, 'Triton',    899,  'moon',         'IAU_TRITON',      1353.4,   0.779, -141.04,  'naif'),

  (999, 'Pluto',     10,   'dwarf_planet', 'IAU_PLUTO',       1188.3,   0.62,  -153.29,  'naif'),
  (901, 'Charon',    999,  'moon',         'IAU_CHARON',       606.0,   0.288, -153.29,  'naif')
ON CONFLICT (body_id) DO NOTHING;

-- Main-belt asteroids (NAIF extended numbering)
INSERT INTO bodies (body_id, name, parent_id, body_class, frame, radius_km, gravity_ms2, ephemeris_src) VALUES
  (2000001, 'Ceres',  10, 'dwarf_planet', 'IAU_CERES',  469.7, 0.27, 'naif'),
  (2000002, 'Pallas', 10, 'asteroid',     'IAU_PALLAS', 256.0, 0.21, 'naif'),
  (2000004, 'Vesta',  10, 'asteroid',     'IAU_VESTA',  262.7, 0.25, 'naif')
ON CONFLICT (body_id) DO NOTHING;


-- ---------------------------------------------------------------------
-- Host stars (synthetic ids 100_001+)
-- ---------------------------------------------------------------------
INSERT INTO bodies (body_id, name, parent_id, body_class, frame, distance_pc, ephemeris_src) VALUES
  (100001, 'Proxima Centauri', NULL, 'star', 'ICRF',  1.301, 'exoplanet_archive'),
  (100002, 'TRAPPIST-1',       NULL, 'star', 'ICRF', 12.43,  'exoplanet_archive'),
  (100003, 'Kepler-22',        NULL, 'star', 'ICRF', 190.0,  'exoplanet_archive'),
  (100004, 'Kepler-186',       NULL, 'star', 'ICRF', 178.5,  'exoplanet_archive'),
  (100005, 'Kepler-452',       NULL, 'star', 'ICRF', 551.0,  'exoplanet_archive'),
  (100006, 'TOI-700',          NULL, 'star', 'ICRF', 31.13,  'exoplanet_archive'),
  (100007, 'HD 209458',        NULL, 'star', 'ICRF', 48.34,  'exoplanet_archive'),
  (100008, '51 Pegasi',        NULL, 'star', 'ICRF', 15.61,  'exoplanet_archive'),
  (100009, 'LHS 1140',         NULL, 'star', 'ICRF', 14.99,  'exoplanet_archive'),
  (100010, 'K2-18',            NULL, 'star', 'ICRF', 38.04,  'exoplanet_archive'),
  (100011, 'Alpha Centauri A', NULL, 'star', 'ICRF',  1.339, 'exoplanet_archive'),
  (100012, 'Alpha Centauri B', NULL, 'star', 'ICRF',  1.339, 'exoplanet_archive')
ON CONFLICT (body_id) DO NOTHING;


-- ---------------------------------------------------------------------
-- Notable exoplanets (synthetic ids 200_001+)
-- ---------------------------------------------------------------------
INSERT INTO bodies (body_id, name, parent_id, body_class, frame, radius_km, ephemeris_src, meta) VALUES
  (200001, 'Proxima Centauri b', 100001, 'exoplanet', 'ICRF', 7160.0,  'exoplanet_archive', '{"habitable_zone":true,"mass_earth":1.07}'::jsonb),
  (200002, 'TRAPPIST-1 b',       100002, 'exoplanet', 'ICRF', 7270.0,  'exoplanet_archive', '{"habitable_zone":false,"mass_earth":1.374}'::jsonb),
  (200003, 'TRAPPIST-1 c',       100002, 'exoplanet', 'ICRF', 6940.0,  'exoplanet_archive', '{"habitable_zone":false,"mass_earth":1.308}'::jsonb),
  (200004, 'TRAPPIST-1 d',       100002, 'exoplanet', 'ICRF', 4960.0,  'exoplanet_archive', '{"habitable_zone":"inner_edge","mass_earth":0.388}'::jsonb),
  (200005, 'TRAPPIST-1 e',       100002, 'exoplanet', 'ICRF', 5800.0,  'exoplanet_archive', '{"habitable_zone":true,"mass_earth":0.692}'::jsonb),
  (200006, 'TRAPPIST-1 f',       100002, 'exoplanet', 'ICRF', 6700.0,  'exoplanet_archive', '{"habitable_zone":true,"mass_earth":1.039}'::jsonb),
  (200007, 'TRAPPIST-1 g',       100002, 'exoplanet', 'ICRF', 7320.0,  'exoplanet_archive', '{"habitable_zone":true,"mass_earth":1.321}'::jsonb),
  (200008, 'TRAPPIST-1 h',       100002, 'exoplanet', 'ICRF', 4900.0,  'exoplanet_archive', '{"habitable_zone":"outer_edge","mass_earth":0.326}'::jsonb),
  (200009, 'Kepler-22 b',        100003, 'exoplanet', 'ICRF', 15280.0, 'exoplanet_archive', '{"habitable_zone":true}'::jsonb),
  (200010, 'Kepler-186 f',       100004, 'exoplanet', 'ICRF', 7480.0,  'exoplanet_archive', '{"habitable_zone":true}'::jsonb),
  (200011, 'Kepler-452 b',       100005, 'exoplanet', 'ICRF', 10180.0, 'exoplanet_archive', '{"habitable_zone":true,"earth_similarity":0.83}'::jsonb),
  (200012, 'TOI-700 d',          100006, 'exoplanet', 'ICRF', 6940.0,  'exoplanet_archive', '{"habitable_zone":true}'::jsonb),
  (200013, 'HD 209458 b',        100007, 'exoplanet', 'ICRF', 95000.0, 'exoplanet_archive', '{"type":"hot_jupiter"}'::jsonb),
  (200014, '51 Pegasi b',        100008, 'exoplanet', 'ICRF', 78400.0, 'exoplanet_archive', '{"type":"hot_jupiter","first_confirmed_around_sunlike":true}'::jsonb),
  (200015, 'LHS 1140 b',         100009, 'exoplanet', 'ICRF', 10920.0, 'exoplanet_archive', '{"habitable_zone":true,"type":"super_earth"}'::jsonb),
  (200016, 'K2-18 b',            100010, 'exoplanet', 'ICRF', 14460.0, 'exoplanet_archive', '{"habitable_zone":true,"atmosphere":"detected"}'::jsonb)
ON CONFLICT (body_id) DO NOTHING;


-- ---------------------------------------------------------------------
-- Surface features — Moon
-- ---------------------------------------------------------------------
INSERT INTO surface_features (body_id, name, feature_type, lat, lon, source, meta) VALUES
  (301, 'Tranquility Base',        'landing_site',   0.6741,  23.4730, 'nasa', '{"mission":"Apollo 11","year":1969}'::jsonb),
  (301, 'Apollo 12 Site',          'landing_site',  -3.0128, -23.4219, 'nasa', '{"mission":"Apollo 12","year":1969}'::jsonb),
  (301, 'Fra Mauro',               'landing_site',  -3.6453, -17.4714, 'nasa', '{"mission":"Apollo 14","year":1971}'::jsonb),
  (301, 'Hadley-Apennine',         'landing_site',  26.1322,   3.6339, 'nasa', '{"mission":"Apollo 15","year":1971}'::jsonb),
  (301, 'Descartes Highlands',     'landing_site',  -8.9734,  15.5011, 'nasa', '{"mission":"Apollo 16","year":1972}'::jsonb),
  (301, 'Taurus-Littrow',          'landing_site',  20.1881,  30.7740, 'nasa', '{"mission":"Apollo 17","year":1972}'::jsonb),
  (301, 'Shackleton Crater',       'crater',       -89.9,      0.0,    'iau_gazetteer', '{"interest":"south_pole_water_ice"}'::jsonb),
  (301, 'Mare Tranquillitatis',    'mare',           8.5,     31.4,    'iau_gazetteer', '{}'::jsonb),
  (301, 'Mare Imbrium',            'mare',          32.8,    -15.6,    'iau_gazetteer', '{}'::jsonb),
  (301, 'Tycho Crater',            'crater',       -43.31,   -11.36,   'iau_gazetteer', '{}'::jsonb),
  (301, 'Copernicus Crater',       'crater',         9.62,   -20.08,   'iau_gazetteer', '{}'::jsonb),
  (301, 'Mons Hadley',             'mons',          26.69,    4.12,    'iau_gazetteer', '{}'::jsonb)
ON CONFLICT (body_id, name) DO NOTHING;

-- Surface features — Mars
INSERT INTO surface_features (body_id, name, feature_type, lat, lon, source, meta) VALUES
  (499, 'Jezero Crater',           'landing_site',  18.4447,  77.4508, 'nasa', '{"mission":"Perseverance","year":2021}'::jsonb),
  (499, 'Gale Crater',             'landing_site',  -5.4,    137.8,    'nasa', '{"mission":"Curiosity","year":2012}'::jsonb),
  (499, 'Meridiani Planum',        'landing_site',  -1.95,   354.47,   'nasa', '{"mission":"Opportunity","year":2004}'::jsonb),
  (499, 'Gusev Crater',            'landing_site', -14.5,    175.4,    'nasa', '{"mission":"Spirit","year":2004}'::jsonb),
  (499, 'Elysium Planitia',        'landing_site',   4.5,    135.6,    'nasa', '{"mission":"InSight","year":2018}'::jsonb),
  (499, 'Utopia Planitia',         'landing_site',  48.27,   225.99,   'nasa', '{"missions":["Viking 2","Zhurong"]}'::jsonb),
  (499, 'Chryse Planitia',         'landing_site',  22.48,   312.03,   'nasa', '{"mission":"Viking 1","year":1976}'::jsonb),
  (499, 'Olympus Mons',            'mons',          18.65,   226.2,    'iau_gazetteer', '{"height_km":21.9,"largest_volcano_in_solar_system":true}'::jsonb),
  (499, 'Valles Marineris',        'vallis',       -13.9,    301.4,    'iau_gazetteer', '{"length_km":4000}'::jsonb),
  (499, 'Tharsis',                 'planitia',       0.0,    260.0,    'iau_gazetteer', '{}'::jsonb)
ON CONFLICT (body_id, name) DO NOTHING;

-- Surface features — Europa / Titan / Ceres
INSERT INTO surface_features (body_id, name, feature_type, lat, lon, source, meta) VALUES
  (502, 'Conamara Chaos',          'chaos',          9.0,    274.0,    'iau_gazetteer', '{"interest":"sub_surface_ocean_candidate"}'::jsonb),
  (606, 'Huygens Landing Site',    'landing_site', -10.34,   167.7,    'nasa',          '{"mission":"Huygens","agency":"ESA/NASA","year":2005}'::jsonb),
  (606, 'Kraken Mare',             'mare',          68.0,    310.0,    'iau_gazetteer', '{"composition":"liquid_methane_ethane"}'::jsonb),
  (606, 'Ligeia Mare',             'mare',          78.0,    249.0,    'iau_gazetteer', '{"composition":"liquid_methane"}'::jsonb),
  (2000001, 'Occator Crater',      'crater',        19.86,   239.34,   'nasa',          '{"interest":"bright_spots_carbonates"}'::jsonb)
ON CONFLICT (body_id, name) DO NOTHING;


-- ---------------------------------------------------------------------
-- AstroNet hops — symbolic starter graph (TIDasToken/kg prices are
-- placeholders; real costs come later from Horizons + launch markets).
-- Bidirectional edges inserted as two rows.
-- ---------------------------------------------------------------------
INSERT INTO astronet_hops (from_body_id, to_body_id, hop_kind, delta_v_kms, transit_days, cost_per_kg, window_rule) VALUES
  -- Earth surface ↔ LEO is modeled body-to-body Earth↔Moon for v1; refine later with orbit nodes
  (399, 301, 'cislunar',       12.4,    3.0,   8500.00, 'always'),
  (301, 399, 'cislunar',        2.7,    3.0,   3200.00, 'always'),

  (399, 499, 'interplanetary', 11.3,  210.0,  18500.00, 'hohmann:26mo'),
  (499, 399, 'interplanetary',  6.1,  210.0,   9800.00, 'hohmann:26mo'),

  (399, 599, 'interplanetary', 14.0,  900.0,  47000.00, 'launch_window:13mo'),
  (399, 599, 'gravity_assist',  9.6,  900.0,  29000.00, 'gravity_assist:venus-earth'),

  (599, 502, 'orbit_insert',    2.1,    5.0,   2400.00, 'always'),    -- Jupiter to Europa
  (599, 503, 'orbit_insert',    1.8,    5.0,   2200.00, 'always'),    -- Jupiter to Ganymede

  (399, 699, 'interplanetary', 15.5, 2200.0,  72000.00, 'launch_window:annual'),
  (699, 606, 'orbit_insert',    2.4,    9.0,   3100.00, 'always'),    -- Saturn to Titan

  (399, 2000001, 'interplanetary', 10.8, 980.0, 26000.00, 'hohmann:15mo')  -- Earth to Ceres
ON CONFLICT (from_body_id, to_body_id, hop_kind) DO NOTHING;
