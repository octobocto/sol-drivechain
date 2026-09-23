// Builds the page into dist/, and the operator scripts into dist/scripts/.
import { build } from "esbuild";
import { cp, mkdir, writeFile, access } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const dist = join(here, "dist");

await mkdir(dist, { recursive: true });

await build({
  entryPoints: [join(here, "src/main.ts")],
  outfile: join(dist, "app.js"),
  bundle: true,
  minify: true,
  sourcemap: true,
  format: "iife",
  target: ["es2022"],
  platform: "browser",
  // web3.js and the token library read Buffer, which a browser does not carry.
  inject: [join(here, "src/buffer-shim.js")],
  define: { global: "globalThis", "process.env.NODE_ENV": '"production"' },
});

await build({
  entryPoints: [join(here, "scripts/seed.ts"), join(here, "scripts/trade.ts")],
  outdir: join(dist, "scripts"),
  bundle: true,
  platform: "node",
  format: "esm",
  target: ["node20"],
  outExtension: { ".js": ".mjs" },
  // The `module` field of web3.js points at its browser build, and that build
  // carries a fetch that fails under node. These two lines keep the node build.
  mainFields: ["main"],
  conditions: ["node", "require"],
  // A node library in the bundle reads `require`, `__filename`, and
  // `__dirname`. An ESM file carries none of them, so the banner makes them.
  banner: {
    js: [
      "import { createRequire } from 'node:module';",
      "import { fileURLToPath as __toPath } from 'node:url';",
      "import { dirname as __toDir } from 'node:path';",
      "const require = createRequire(import.meta.url);",
      "const __filename = __toPath(import.meta.url);",
      "const __dirname = __toDir(__filename);",
    ].join(""),
  },
});

for (const name of ["index.html", "style.css"]) {
  await cp(join(here, "src", name), join(dist, name));
}

// A deployment names its own chain. The build writes the file one time, so a
// later edit on the server stays.
const configPath = join(dist, "config.json");
try {
  await access(configPath);
} catch {
  await writeFile(
    configPath,
    `${JSON.stringify({ rpcUrl: "http://127.0.0.1:8799", tokens: [] }, null, 2)}\n`,
  );
}

console.log(`the page is in ${dist}`);
