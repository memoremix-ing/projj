import type { ElectrobunConfig } from "electrobun";

// The page is projj's own web client, unchanged: `web` links to ../web,
// since Electrobun only copies files from inside the project. read-aloud's
// modules are read by the page from /read-aloud/.
const page = Object.fromEntries(
	[
		"index.html", "app.js", "highlight.js", "style.css",
		...["collect", "controls", "read-aloud", "reader", "speech", "text", "voices"].map((name) => `read-aloud/${name}.js`),
	].map((name) => [`web/${name}`, `views/projj/${name}`]),
);

// CEF's remote debugging, which Electrobun turns on for dev builds, listens on
// loopback with no sign-in: any process on the device could drive the page,
// and through it the API. Off unless asked for, for one build:
//
//   PROJJ_CEF_DEBUG_PORT=9222 hutch run dev
const engine = {
	bundleCEF: true,
	// One engine everywhere, the one the browser tests drive.
	defaultRenderer: "cef",
	chromiumFlags: {
		"remote-debugging-port": process.env.PROJJ_CEF_DEBUG_PORT || false,
		// Edit > Read Aloud starts speech from the app's menu, not from a click
		// in the page, and Chromium speaks unasked only where media may play
		// unasked. The page has no media to play.
		"autoplay-policy": "no-user-gesture-required",
	},
} as const;

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
		mac: engine,
		linux: engine,
		win: engine,
	},
	runtime: {
		exitOnLastWindowClosed: true,
	},
} satisfies ElectrobunConfig;
