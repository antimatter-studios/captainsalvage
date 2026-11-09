### Fixed

- **Every file carries one version number.** The version lived in six places, and only a release
  set them, so the v0.2.0 tag shipped with manifests that still said 0.1.0. It now lives in `VERSION`;
  `release.py sync` copies it everywhere and `release.py check` runs on every pull request. The build
  carries that number, not one worked out from unreleased pull requests.
