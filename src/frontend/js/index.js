function getRepoName() {
  return window.location.pathname.split("/").filter(Boolean).join("/") || null;
}

function formatSize(bytes) {
  const mb = bytes / (1024 * 1024);
  return mb.toFixed(2) + " MB";
}

function getCookie(name) {
  const match = document.cookie.match(new RegExp('(^| )' + name + '=([^;]+)'));
  return match ? decodeURIComponent(match[2]) : null;
}

function createLayerHTML(layers) {
  return layers.map(l => `
    <div class="layer">
      ${l.digest} — ${formatSize(l.size)}
    </div>
  `).join("");
}

function createTagCard(tag) {
  const totalSize = tag.layers.reduce((acc, l) => acc + l.size, 0);

  // Pull metadata from annotations (OCI) or fall back to tag.info (Docker v2)
  const annotations = tag.annotations || {};
  const info = tag.info || {};

  const created = annotations["org.opencontainers.image.created"]
    || info.created
    || null;

  const baseImage = annotations["org.opencontainers.image.base.name"]
    || info.architecture
    || "N/A";

  const architecture = info.architecture || "N/A";
  const os = info.os || "N/A";

  // Detect manifest type
  const isOCI = tag.mediaType && tag.mediaType.includes("oci");

  return `
    <div class="card">
      <div class="tag">
        ${tag.name}
        ${isOCI ? '<span class="badge">OCI</span>' : '<span class="badge">Docker v2</span>'}
        ${architecture !== "N/A" ? `<span class="badge">${architecture}</span>` : ""}
      </div>

      <div class="meta">
        ${created     ? `Created: ${new Date(created).toLocaleString()} <br>` : ""}
        ${os !== "N/A"          ? `OS: ${os} <br>` : ""}
        ${baseImage             ? `Base Image: ${baseImage} <br>` : ""}
        Size: ${formatSize(totalSize)} <br>
        Digest: ${tag.digest || "N/A"}
      </div>

      <h3>Layers</h3>
      <div>
        ${createLayerHTML(tag.layers)}
      </div>
    </div>
  `;
}

async function loadRepo() {
  const repo = getRepoName();
  const repoEl = document.getElementById("repoName");
  const contentEl = document.getElementById("content");

  if (!repo) {
    repoEl.innerText = "No repository found";
    return;
  }

  repoEl.innerText = repo;

  try {
    const token = getCookie("access_token");
    const res = await fetch(`/registry-ui/api/repos/${repo}?full=true`, {
      headers: { Authorization: `Bearer ${token}` },
    });

    if (!res.ok) throw new Error("API error");

    const data = await res.json();

    if (!data.tags || data.tags.length === 0) {
      contentEl.innerHTML = `<p>No tags found for this repository.</p>`;
      return;
    }

    contentEl.innerHTML = data.tags.map(createTagCard).join("");

  } catch (err) {
    console.error(err);
    contentEl.innerHTML = `<p>Failed to load repository</p>`;
  }
}

document.addEventListener("DOMContentLoaded", loadRepo);