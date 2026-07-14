# Waldo

## Cross-machine adaptability

Build Waldo from runtime-discovered capabilities and reported desktop state so
it remains useful across machines. Derive behaviour from output geometry,
scale, transforms, keyboard layout, compositor capabilities, and application
state. Keep workstation-specific identifiers, paths, timings, applications,
and display layouts out of product logic; when a genuine machine difference
needs policy, expose it as a discovered capability or explicit configuration.
