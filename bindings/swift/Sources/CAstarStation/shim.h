/*
 * shim.h — the umbrella header for the `CAstarStation` SwiftPM system-library
 * target. It pulls in the committed, cbindgen-generated C-ABI header
 * (crates/astar-sys/include/astar.h) by relative path so there is a
 * single source of truth: regenerating astar.h is automatically picked up
 * here, no copy to keep in sync.
 *
 * The relative path walks up from
 *   bindings/swift/Sources/CAstarStation/  (this file)
 * to the repo root, then into the astar-sys crate's include dir.
 */
#include "../../../../crates/astar-sys/include/astar.h"
