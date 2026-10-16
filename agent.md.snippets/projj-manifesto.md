We want to manage ALL code across all projects in a single consistent way.  We anticipate that EVERY project will eventually need to be worked on in parallel with the projects upon which it will depend.

We aspire to manage ALL inter-project dependencies in a single consistent way regardless of language or runtime environment.  We hope that language-agnostic version control systems can serve as our foundation.

We recognize that the (frequently agentic) needs of version control when authoring code can clash with the (occasionally human) needs of version control when reviewing code.

We believe that predictable relative folder/file arrangements based around remote tracking branches whose commits beyond the branch point include no "breaking changes" will become the driver for a source-first, language-agnostic dependency and package management system.

We suspect that, while multi-person code collaboration workflow experiences used to primarily be a concern for "remote-first" code forges that connected humans who had exclusive access to development machines, complementary workflow experiences must be made available "local-first" for multi-agent/human teams to effectively collaborate on code when using the same machine(s).

---

The root `./.git` repository is for HUMAN USE ONLY. Agents must use jj (Jujutsu) for agent-driven version control under the `track/` directory.

Repositories map to their actual or anticipated upstream URLs (for example, `track/github.com/${tenant}/${repo}.git/`).  Every `track/.../.repo/` directory contains repository metadata: never edit it or use it for development.  Always develop in sibling workspaces, such as `track/github.com/${tenant}/${repo}.git/sts-0/` or `track/github.com/${tenant}/${repo}.git/lts-1/`, which track the corresponding remote branches that ought to preserve ("major-version") API compatibility.  While working from any remote branch is technically possible, the provisional naming convention is to prefix branch names with "lts" when they can be expected to offer "long-term support" (typically on "odd-numbered" releases) or "sts" when only "short-term support" should be expected (typically on "even-numbered" releases).
