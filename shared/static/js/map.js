// Talk to whichever outpost served this page.
const API = location.origin + "/api";

// Fabric routes are guarded; reuse the token the console stored.
function authHeaders() {
  const t = localStorage.getItem("tidw_token");
  if (!t) return {};
  return (t.split(".").length === 3)
    ? { Authorization: "Bearer " + t }
    : { "X-Node-Token": t };
}

// Map setup
const map = L.map('map').setView([20, 0], 2);

L.tileLayer('https://{s}.tile.openstreetmap.org/{z}/{x}/{y}.png', {
  maxZoom: 19
}).addTo(map);

// ─── Fleet movement trails ("transcripts") ───────────────────────────────
const trails = new Map();
const TRAIL_COLORS = ['#38bdf8','#34d399','#fbbf24','#f87171','#a78bfa','#f472b6'];

async function loadTracks() {
  try {
    const res = await fetch(`${API}/map/fleet/tracks?hours=24`, { headers: authHeaders() });
    if (!res.ok) return;
    const gj = await res.json();
    const seen = new Set();
    gj.features.forEach((f, i) => {
      const id = f.properties.asset_id;
      seen.add(id);
      const latlngs = f.geometry.coordinates.map(([lon, lat]) => [lat, lon]);
      const color = TRAIL_COLORS[i % TRAIL_COLORS.length];
      if (trails.has(id)) {
        trails.get(id).setLatLngs(latlngs);
      } else {
        trails.set(id, L.polyline(latlngs, { color, weight: 2, opacity: 0.65 }).addTo(map));
      }
    });
    for (const [id, line] of trails.entries()) {
      if (!seen.has(id)) { map.removeLayer(line); trails.delete(id); }
    }
  } catch (e) { console.error("Track load error:", e); }
}

const markers = new Map();

function iconFor(type) {
  const color = ({
    satellite: '#0af',
    drone: '#08c',
    ev: '#4a4',
    server: '#999',
    honeypot: '#c80'
  }[type] || '#555');

  return L.divIcon({
    className: 'tid-pin',
    html: `
      <div style="
        width:14px;
        height:14px;
        border-radius:50%;
        background:${color};
        border:2px solid white;
        box-shadow:0 0 2px rgba(0,0,0,.6)
      "></div>`,
    iconSize: [14, 14]
  });
}

async function load() {
  try {
    const res = await fetch(`${API}/map/fleet/geojson`, { headers: authHeaders() });
    const gj = await res.json();
    const ids = new Set();

    gj.features.forEach(f => {
      const p = f.properties || {};
      const [lon, lat] = f.geometry.coordinates;
      const id = p.asset_id || Math.random().toString(36).slice(2);
      ids.add(id);

      const content = `
        <div class="popup">
          <h4>${p.name || 'Asset'}</h4>
          <div><b>Type:</b> ${p.asset_type || '-'}</div>
          <div><b>Node:</b> ${p.node_id || '-'}</div>
          <div><b>Time:</b> ${p.timestamp || '-'}</div>
          <div><b>Battery:</b> ${p.battery ?? '-'}%</div>
          <div><b>Signal:</b> ${p.signal_db ?? '-'} dB</div>
          ${
            p.anomaly_score && p.anomaly_score > 0.8
            ? `<div class="anomaly">⚠ Anomaly: ${p.anomaly_score.toFixed(2)}</div>`
            : ''
          }
          ${p.malware_flag ? `<div class="anomaly">🛑 Malware flag</div>` : ''}
        </div>
      `;

      if (markers.has(id)) {
        const m = markers.get(id);
        m.setLatLng([lat, lon]);
        m.setIcon(iconFor(p.asset_type));
        m.bindPopup(content);
      } else {
        const m = L.marker([lat, lon], { icon: iconFor(p.asset_type) })
          .addTo(map)
          .bindPopup(content);
        markers.set(id, m);
      }
    });

    // Remove stale markers
    for (const [id, m] of markers.entries()) {
      if (!ids.has(id)) {
        map.removeLayer(m);
        markers.delete(id);
      }
    }

  } catch (e) {
    console.error("Map load error:", e);
  }
}

load();
loadTracks();
setInterval(load, 5000);
setInterval(loadTracks, 15000);

