// The download button's default href (in index.html) points at the
// Releases page as a safe fallback. If this fetch succeeds, it's upgraded
// to a direct link to the latest framewire.exe asset and the version is
// shown — no build step or redeploy needed when a new version ships.
const downloadButton = document.getElementById('download-button');
const versionLabel = document.getElementById('version-label');

fetch('https://api.github.com/repos/Hinode-dev/Framewire/releases/latest')
  .then((res) => (res.ok ? res.json() : Promise.reject()))
  .then((release) => {
    const asset = release.assets.find((a) => a.name === 'framewire.exe');
    if (asset) {
      downloadButton.href = asset.browser_download_url;
    }
    versionLabel.textContent = release.tag_name ? `Latest: ${release.tag_name}` : '';
  })
  .catch(() => {});
