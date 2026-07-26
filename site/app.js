// Populated by the release workflow on every tagged release: download/version.txt
// holds the tag name, download/framewire.exe is the matching build. Until the
// first release runs, these 404 and the button falls back to the GitHub
// Releases page (its default href in index.html).
const downloadButton = document.getElementById('download-button');
const versionLabel = document.getElementById('version-label');

fetch('download/version.txt', { cache: 'no-store' })
  .then((res) => (res.ok ? res.text() : Promise.reject()))
  .then((version) => {
    version = version.trim();
    downloadButton.href = 'download/framewire.exe';
    versionLabel.textContent = version ? `Latest: ${version}` : '';
  })
  .catch(() => {});
