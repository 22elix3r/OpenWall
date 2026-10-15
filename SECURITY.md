# Security

This development snapshot is not a hardened runtime for arbitrary downloaded wallpapers.
The GUI and playback are separate processes, but OS sandboxing, worker resource quotas,
script/browser sandboxes and a complete dependency audit remain outstanding.

Readers bound file, package, image and scene sizes and reject escaping paths. Imported
scripts, native application wallpapers, custom shader code and web pages are not executed.
The renderer currently accepts only the documented image-layer subset and its own shader.

Please report vulnerabilities using a private GitHub security advisory:
https://github.com/22elix3r/OpenWall/security/advisories/new

Include a minimal original/reproducible fixture, affected version, OS/compositor/GPU and
impact. Do not publish private files, access tokens or third-party copyrighted content.
There is no stable release/support window yet; fixes target the development branch.
