# Metal draw fix for macOS Guests

The macOS Guest's paravirtual Metal device ("Apple Paravirtual device") gets
`baseVertex` wrong in direct indexed draws
(`drawIndexedPrimitives:indexCount:…:baseVertex:baseInstance:`). It reads the value
as an unsigned 16-bit number, so only 0…65535 arrive intact: 65536 draws from
vertex 0, and -1 from vertex 65535. Indirect indexed draws carry any value
correctly. Renderers that pack many meshes into one vertex buffer hit this
constantly. MoltenVK passes Vulkan's `vertexOffset` straight through, so
SuperTuxKart's Vulkan renderer draws stretched triangles across the screen. The
same build renders correctly on the host.

`metalfix.dylib` is injected into an app with `DYLD_INSERT_LIBRARIES`. It
replaces the direct indexed draw on the device's render encoder, so each call
writes its arguments into a 20 MiB ring buffer and draws indirectly from
there. It rewrites every direct indexed draw, not only the out-of-range ones.
With only those rewritten, SuperTuxKart still showed white stripes in some
frames, and in-Guest shader validation still reported out-of-bounds vertex
fetches. Neither went away until all of them were rewritten. That second fault
has not been reproduced in isolation. The library does nothing on any other
device unless `METALFIX_FORCE=1` is set.

## Build and check

```sh
native/guest-metal-fix/build.sh            # .local/bin/metalfix.dylib, .local/bin/metal-probe
veynodevm cp .local/bin/metal-probe mac:/Users/Shared/metal-probe
veynodevm cp .local/bin/metalfix.dylib mac:/Users/Shared/metalfix.dylib
veynodevm exec mac /Users/Shared/metal-probe 4000
veynodevm exec mac env DYLD_INSERT_LIBRARIES=/Users/Shared/metalfix.dylib /Users/Shared/metal-probe 4000
```

`metal-probe` issues random direct indexed draws and compares the vertex and
instance ids each one produced with the expected ones. On an M3 Pro host and on
a macOS 26.6 Guest, the results were:

| Device | Draws | Wrong |
| --- | --- | --- |
| Host | 4000 | 0 |
| Guest | 4000 | 780 |
| Guest with the library | 4000 | 0 |

## Running an app with it

`DYLD_INSERT_LIBRARIES` is ignored for apps with the hardened runtime, which
covers most notarized apps. Make an ad-hoc signed copy first, then start it in
the login session as the logged-in user. `launchctl asuser` alone runs it as
root, so the inner `sudo -u` is needed:

```sh
veynodevm exec mac ditto /Applications/Game.app /Users/Shared/Game.app
veynodevm exec mac codesign --force --deep --sign - /Users/Shared/Game.app
veynodevm exec mac sh -c 'sudo -n launchctl asuser $(id -u) sudo -n -u "#$(id -u)" env DYLD_INSERT_LIBRARIES=/Users/Shared/metalfix.dylib /Users/Shared/Game.app/Contents/MacOS/Game'
```

The library prints `[metalfix] drawing direct indexed draws as indirect draws`
to stderr on its first rewrite.

SuperTuxKart 1.5 run this way with `--render-driver=vulkan` (bundled MoltenVK
1.3.0) renders its `--benchmark` race like the host at 1920x1080, typical
59 fps. Without the library the race also runs at 59 fps, but its 3D scenes are
corrupted. Apps that draw through Metal directly hit the first fault whenever
they pass a `baseVertex` outside 0…65535 to a direct draw. The second fault has
only been seen through MoltenVK.
