#!/usr/bin/env electron
"use strict";

const path = require("node:path");
const { app, BrowserWindow } = require("electron");

const SCHEMA_VERSION = 1;
const FIXTURE_VERSION = "1";

function argumentsFrom(argv) {
  const values = {};
  for (let index = 0; index < argv.length; index += 1) {
    const item = argv[index];
    if (!item.startsWith("--")) continue;
    const name = item.slice(2);
    if (name === "probe-api") {
      values[name] = true;
      continue;
    }
    values[name] = argv[index + 1];
    index += 1;
  }
  return values;
}

function descriptor(seed) {
  return {
    schema_version: SCHEMA_VERSION,
    fixture_version: FIXTURE_VERSION,
    toolkit: "electron",
    display_route: "native_wayland",
    seed,
    title: `Waldo Electron Interaction Oracle ${seed.toString(16).padStart(8, "0")}`,
    capabilities: {
      pointer: true,
      keyboard: true,
      text: true,
      scroll: true,
      drag: true,
      delayed_state: true,
      final_consumer_log: true,
    },
  };
}

const options = argumentsFrom(process.argv.slice(2));
const seed = Number.parseInt(options.seed, 10);
if (!Number.isSafeInteger(seed)) {
  process.stderr.write("--seed must be an integer\n");
  process.exit(2);
}

if (options["probe-api"]) {
  process.stdout.write(`${JSON.stringify(descriptor(seed), null, 2)}\n`);
  process.exit(0);
}

for (const required of ["state", "events", "control"]) {
  if (!options[required]) {
    process.stderr.write(`--${required} is required outside --probe-api\n`);
    process.exit(2);
  }
}

app.commandLine.appendSwitch("ozone-platform", "wayland");

app.whenReady().then(() => {
  const fixture = descriptor(seed);
  const window = new BrowserWindow({
    width: 920,
    height: 700,
    title: fixture.title,
    webPreferences: {
      contextIsolation: false,
      nodeIntegration: true,
      sandbox: false,
    },
  });
  window.setTitle(fixture.title);
  window.loadFile(path.join(__dirname, "interaction_electron.html"), {
    query: {
      state: options.state,
      events: options.events,
      control: options.control,
      descriptor: JSON.stringify(fixture),
    },
  });
});

app.on("window-all-closed", () => app.quit());
