# Desktop UI draft (Simple edition)

Images of the desktop design draft, the "Simple edition" page of the Design canvas (sixteen boards). They are a reference for the desktop issues of #527 (epic #523), not a specification: wording, density and sample data are drafts. The editable canvas is not in this repository.

Drawn for people who are not technical: plain words, one window on the local machine, technical detail behind "Show technical details". The look uses the dark tokens of the design canvas (background `#0F1113`, accent `#5CC8B0`, Inter and JetBrains Mono; they are not defined anywhere in the repository yet, #1028 creates the token module). Rows and buttons are about 34 to 38 px; boards S1, S11 and S12 were drawn first with 44 px controls and not redrawn.

Rendered from the board HTML with Chromium at 1x. They were not rendered by GPUI, and nothing here was built; check each part in a small spike (the GPUI Kit names on S13 were read from the source of gpui-component 0.9.1, not compiled).

| Board | Screen | Issue |
|---|---|---|
| [S1](s1-setup.png) | First setup, two screens | #1037 |
| [S2](s2-home.png) | Home with the sidebar | #1030 |
| [S3](s3-task.png) | A task in progress | #1031 |
| [S4](s4-allow.png) | A task waits for an OK | #1032 |
| [S5](s5-approvals.png) | Approvals page | #1032 |
| [S6](s6-automations.png) | Automations page | #1033 |
| [S7](s7-skills.png) | Skills, review before use | #1034 |
| [S8](s8-tools.png) | Connected tools, trust | #1034 |
| [S9](s9-settings.png) | Settings: Safety | #1035 |
| [S10](s10-general.png) | Settings: General | #1035 |
| [S11](s11-kinds.png) | Three kinds of requests | #1037 |
| [S12](s12-moments.png) | Question, problem, done, welcome back | #1037 |
| [S13](s13-map.png) | Plain words and GPUI Kit parts | all |
| [S14](s14-discuss.png) | Discussion report (Get opinions) | #1036 |
| [S15](s15-discussions.png) | Discussions page and new discussion | #1036 |
| [S16](s16-helpers.png) | Settings: Helpers (profiles) | #1035 |

## Intended differences from the technical clients

- Approvals are a page and a card, answered "Allow once" or "Don't allow"; no "always allow" for a single request (the serve decision route is one-shot, and Destructive tools are never auto-allowed by the core).
- Discussions have their own menu: the owner chooses the helpers, or Tole proposes them and the owner confirms before anything runs (a model-started discussion is an approval).
- Skills and connected tools: trust approval from the app is an open owner decision (#1034); nothing a model says can trust a tool.
- Automations run while the local serve runs, if it was started with the cron scheduler (`--cron`); closing the window does not stop them.

See [../../desktop-design.md](../../desktop-design.md) section 0 for the architecture decisions.
