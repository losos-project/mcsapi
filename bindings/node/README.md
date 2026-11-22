# mcsapi for TypeScript

Node.js bindings for the mcsapi desktop policy, built with
[napi-rs](https://napi.rs). The Rust crate stays the single source of truth:
TypeScript calls straight into it through a native addon, and `index.d.ts` is
generated from the Rust signatures, so types and behavior cannot drift.

This lets a TypeScript program act as the policy engine: it decides what
happens on window map/unmap and keybindings, and receives the logical geometry
to apply. Surfaces, input, and rendering stay with the compositor host, exactly
as with the Rust API.

## Build

Requires Rust 1.95+, Node.js 22.18+, and `libxkbcommon-dev` on Debian/Ubuntu.

```sh
npm install
npm run build      # release addon: mcsapi.<platform>.node, index.js, index.d.ts
npm test           # node:test suite, written in TypeScript
npm run typecheck
npm run example    # headless engine driven by scripted events
```

## Usage

```ts
import { Desktop, arrange } from 'mcsapi'

const desktop = new Desktop([1, 2, 3])
desktop.insert(10)
desktop.insert(11)
desktop.focusPrevious()
desktop.promoteFocused()
// A new workspace uses 'spiral'. setLayout selects 'spiral', 'tall', or 'monocle'.
desktop.setLayout('tall')

for (const { window, geometry } of desktop.arrange({ x: 0, y: 0, width: 1920, height: 1080 })) {
  // configure the host surface mapped to `window`
}

// Layouts work without a Desktop, for previews or custom membership tracking.
arrange('spiral', { x: 0, y: 0, width: 800, height: 600 }, [1, 2, 3])
arrange('monocle', { x: 0, y: 0, width: 800, height: 600 }, [1, 2])
```

`Desktop` mirrors the Rust methods in camelCase: `insert`, `remove`,
`moveWindow`, `switchTo`, `focus`, `focusNext`, `focusPrevious`,
`promoteFocused`, `setLayout`, and `arrange`. `active()` and `workspaces()`
return plain snapshot objects; `workspaceOf(window)` finds a window's
workspace. The layout names are `'spiral'` (the default on a new workspace),
`'tall'`, and `'monocle'`. Spiral splits the remaining area alternately left
and top, with the last window filling whatever is left; tall puts one main
window in the left half (half the width, rounding down) and stacks the others
on the right; monocle gives every window the same bounds, and the host
displays only the focused one. A single window fills the output in every
layout. For tall with more than one window, a width under 2 or a stack taller
than the output throws `InsufficientSpace`.

IDs are JavaScript numbers and must be integers from 1 to
`Number.MAX_SAFE_INTEGER`. Errors are thrown as `Error` objects whose `code`
is the Rust variant name (`UnknownWindow`, `DuplicateWindow`,
`UnknownWorkspace`, `DuplicateWorkspace`, `NoWorkspaces`, `InvalidGeometry`,
`InsufficientSpace`), or `InvalidArg` for a malformed ID. A failed call leaves
the desktop unchanged.

The egui/GPUI toolkit and widget modules are not bound: they hand out Rust UI
objects that have no meaning in a JavaScript runtime.
