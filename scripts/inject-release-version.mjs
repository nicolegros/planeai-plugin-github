import fs from "node:fs";

const [tag] = process.argv.slice(2);
if (!tag || !/^v\d+\.\d+\.\d+$/.test(tag)) {
  throw new Error("usage: node scripts/inject-release-version.mjs v<major>.<minor>.<patch>");
}

const version = tag.slice(1);
const replaceExactlyOnce = (path, pattern, replacement) => {
  const source = fs.readFileSync(path, "utf8");
  const flags = pattern.flags.includes("g") ? pattern.flags : `${pattern.flags}g`;
  const matches = [...source.matchAll(new RegExp(pattern.source, flags))];
  if (matches.length !== 1) {
    throw new Error(`expected exactly one release-version placeholder in ${path}`);
  }
  fs.writeFileSync(path, source.replace(pattern, replacement));
};

const rewriteJsonVersion = (path) => {
  const document = JSON.parse(fs.readFileSync(path, "utf8"));
  if (document.version !== "0.0.0") {
    throw new Error(`expected ${path} to contain the 0.0.0 release-version placeholder`);
  }
  document.version = version;
  fs.writeFileSync(path, `${JSON.stringify(document, null, 2)}\n`);
};

rewriteJsonVersion("package.json");
rewriteJsonVersion("planeai-plugin.json");
replaceExactlyOnce("Cargo.toml", /^version = "0\.0\.0"$/m, `version = "${version}"`);
replaceExactlyOnce(
  "Cargo.lock",
  /(\[\[package\]\]\nname = "planeai-plugin-github"\nversion = )"0\.0\.0"/,
  `$1"${version}"`,
);

console.log(`Injected release version ${version}`);
