# Source context

Purpose: Index the Rust desktop-control modules and workflow boundary.

## Vocabulary

- Domain: Linux desktop observation and control.
- Bounded context: the CLI and session-scoped MCP server.
- Policy: script execution limits and desktop-tool allowlist.
- Shared kernel: accessibility nodes, window targets, and screenshot metadata.

## Files

- `lib.rs`: Purpose: Wire internal modules and expose supported library APIs.
- `main.rs`: Purpose: Start the main CLI with the selected allocator.
- `server.rs`: Purpose: Define MCP tools and dispatch desktop operations.
- `run-script.rs`: Purpose: Execute bounded Rhai desktop workflows without host-code access.
- `desktop-workflows.rs`: Purpose: Run a desktop action and return fresh scoped observation feedback.
- `tool-output.rs`: Purpose: Keep screenshot payloads out of JSON and retain native script images.
- `cli.rs`: Purpose: Parse and execute CLI commands.
- `abs_pointer.rs`: Purpose: Provide an absolute uinput pointer backend.
- `accessibility_guard.rs`: Purpose: Run the optional foreground GNOME accessibility guard.
- `atspi_tree.rs`: Purpose: Observe and mutate AT-SPI accessibility objects.
- `command_runner.rs`: Purpose: Execute bounded subprocess operations.
- `cosmic_helper.rs`: Purpose: Serve COSMIC window-control requests.
- `diagnostics.rs`: Purpose: Report desktop readiness and accessibility setup.
- `gnome_extension.rs`: Purpose: Install and query the GNOME window-targeting extension.
- `identity.rs`: Purpose: Share GNOME integration identifiers.
- `remote_desktop.rs`: Purpose: Manage portal-backed keyboard and pointer input.
- `screenshot.rs`: Purpose: Capture and bound screenshot payloads.
- `terminal.rs`: Purpose: Identify terminal processes and paste behavior.
- `windows.rs`: Purpose: Resolve window targets and focus operations.
- `ydotool.rs`: Purpose: Discover and validate the ydotool input backend.

## Subdomains

- `windowing/`: compositor backend registry, target resolution, and geometry.
- `bin/`: the COSMIC helper executable entry point.

## Workflow boundary

`run_script` uses the existing server instance, so accessibility indices and input backends are shared with standalone tools. Its dispatcher excludes shell execution, script recursion, and completion notifications. Scripts are sequential, not desktop transactions; completed actions survive failure and cancellation. Image-bearing calls replace payloads with script-local handles before entering Rhai. Native image blocks are attached only when emitted metadata references those handles. `act_and_observe` separates completed input from fresh state and requires effect verification. Unknown window targets refuse an unscoped accessibility-tree fallback.
