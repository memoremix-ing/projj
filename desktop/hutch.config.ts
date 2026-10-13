// Hutch drives Electrobun's build: `hutch run dev` to try the app out,
// `hutch run build` for an installable one. See README.md.
export default {
	scripts: {
		dev: ["hutch", "electrobun", "dev"],
		build: ["hutch", "electrobun", "build", "--env=stable"],
	},
	electrobun: {
		version: "2.0.2",
	},
};
