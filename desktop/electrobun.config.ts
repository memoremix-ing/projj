import type { ElectrobunConfig } from "electrobun";

// The page is projj's own web client, unchanged: `web` links to ../web,
// since Electrobun only copies files from inside the project.
const page = Object.fromEntries(
	["index.html", "app.js", "highlight.js", "style.css"].map((name) => [`web/${name}`, `views/projj/${name}`]),
);

export default {
	app: {
		name: "projj",
		identifier: "com.memoremix.projj",
		version: "0.1.0",
		description: "Review what agents changed, one hunk at a time.",
	},
	build: {
		mainProcess: "rust",
		rust: {
			// jj-lib needs 1.89 or later; Hutch's default is older.
			version: "1.92.0",
			manifest: "Cargo.toml",
			binary: "main",
		},
		copy: page,
		// One engine everywhere, the one the browser tests drive.
		mac: { bundleCEF: true, defaultRenderer: "cef" },
		linux: { bundleCEF: true, defaultRenderer: "cef" },
		win: { bundleCEF: true, defaultRenderer: "cef" },
	},
	runtime: {
		exitOnLastWindowClosed: true,
	},
} satisfies ElectrobunConfig;
