// The download button (in index.html) always points directly at
// download/framewire.exe, served straight from this same server — drop a
// new build there to publish it, no rebuild or redeploy needed.
//
// version.txt is optional: if it's placed alongside framewire.exe, its
// contents are shown next to the button. Missing it just hides the label.
const versionLabel = document.getElementById('version-label');

fetch('download/version.txt', { cache: 'no-store' })
  .then((res) => (res.ok ? res.text() : Promise.reject()))
  .then((version) => {
    version = version.trim();
    versionLabel.textContent = version ? `Latest: ${version}` : '';
  })
  .catch(() => {});
