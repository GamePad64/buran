# AGENTS.md — buran

_Лёгковесный агентский харнесс на Rust в стиле Pi: ACP-агент + MCP-инструменты, один бинарник, ноль встроенных тулов._

## Что это за проект

**buran** (`cargo bin`: `buran`, короткая команда `bu`) — headless ACP-сервер агента с отдельной TUI-плашкой. Один процесс = одна сессия. Назван в честь «Бурана» (15.11.1988 — первый корабль, взлетевший, выполнивший миссию и севший полностью автономно).

Проект стартует с нуля — **в репо пока нет кода**. Твоя задача: построить MVP по спеке ниже. Спека выстраданная, дизайн-раунды закрыты; не пересматривай архитектурные решения, помеченные «FIXED» — они согласованы с владельцем.

## Философия (читать до старта)

1. **Минимализм в стиле Pi.** Никаких встроенных инструментов агента. Всё снаружи, через MCP. Если функция можно выразить конфигом или MCP-сервером — она не попадает в код харнесса.
2. **Один бинарник, ноль рантайм-зависимостей.** Пользователь ставит один файл — и всё работает. Никаких node/python для базовой работы.
3. **Провайдеры — конфиг, не код.** Только OpenAI-compatible endpoints. Разница между провайдерами — `base_url`, `api_key`, имя модели. Никаких per-provider плагинов.
4. **WASM-плагины — нет (v1).** MCP-over-stdio уже является кросс-языковой системой плагинов. wasmtime добавил бы десятки мегабайт и противоречил цели «один бинарник».

## Архитектура (FIXED — не пересматривать)

Три уровня, каждый со своим владельцем:

| Уровень | Протокол | Что делает |
|---|---|---|
| Инструменты/данные | **MCP** (JSON-RPC, stdio) | агенты ↔ тулы. Любые тулы — только MCP-серверы, включая ACT |
| Клиент ↔ агент | **ACP** (agent-client-protocol, stdio) | редактор/клиент ↔ агент: сессии, стрим `session/update`, запросы разрешений |
| Агент ↔ агент (v3) | **A2A** | сетевая федерация, скрытые внутренности |

Ключевое правило ACP: клиент может вызвать только `initialize / newSession / loadSession / prompt / stop / setSessionMode / setSessionModel`. Инструмент на агенте клиент вызвать не может — двигает агента только `prompt`. Файловая система, терминал, permissions — на стороне **клиента** (`fs/read_text_file`, `fs/write_text_file`, `terminal/create`, `session/request_permission`); LLM-цикл и исполнение tool-calls — на стороне **агента**.

## Обязательные поверхности (FIXED — минимум MVP)

1. **AGENTS.md** — инструкции проекта, читаются харнессом автоматически (как Claude Code читает CLAUDE.md). Формат совместим с гейтвейным AGENTS.md; CLAUDE.md — симлинк.
2. **`.mcp.json`** — конфиг MCP-серверов проекта (де-факто стандарт формата).
3. **Skills**: `SKILL.md` с frontmatter (`name`, `description`) — описания инжектятся в системный промпт, тело подгружается по релевантности. Формат совместим с OpenClaw/Anthropic skill-creator. Нативно + skills-over-MCP (удалённые источники скиллов подключаются как MCP).
4. **ACT как рекомендуемый дефолт**: в docs и first-run flow рекомендовать ACT (MCP-over-stdio) как готовый слой инструментов. **В коде — ни одной ветки про ACT.**

## Скоуп MVP

Реализовать:

1. **`buran serve`** — headless ACP-сервер (stdio): `initialize`, `newSession`, `prompt`, `stop`, стрим `session/update`, permission-запросы наружу.
2. **LLM-цикл**: OpenAI-compatible endpoint (base_url + api_key + model из конфига), стриминг, tool-call loop против подключённых MCP-серверов.
3. **MCP-клиент**: спавн stdio-серверов из `.mcp.json`, tools/list → tools/call. Использовать `rmcp` (официальный Rust MCP SDK).
4. **Конфиг**: провайдеры + MCP-серверы + скиллы (глобальный `~/.config/buran/config.toml` + проектный `.buran/` или общий проектный формат — реши по ходу, главное — проектные MCP-серверы читаются из `.mcp.json`).
5. **Skills loader**: сканирует локальные директории скиллов, парсит frontmatter, инжектит описания, отдаёт тело по запросу.
6. **`buran tui`** — отдельный бинарник (или feature), TUI-клиент поверх ACP: ratatui. Минимум: сессия, стрим текста, approve/deny permission. TUI — отдельная витрина, не часть `serve`.
7. **First-run doctor**: на старте проверить MCP-серверы из конфига; ни одного — предложить ACT как дефолтный набор инструментов (интерактивно, без кодовой ветки).
8. **Graceful degradation ACP**: если клиент в `initialize` не объявил fs/terminal-капабилити — агент обязан откатиться на bundled filesystem MCP-сервер (включается одной строчкой конфига) или честно потерять файловые инструменты, не падая.

Явно **не** в MVP: субагенты (v2 — см. ниже), A2A (v3), WASM-плагины (отклонено), компакция контекста (см. Open questions), A2A-обнаружение.

## Крейты (не тянуть лишнего)

- `agent-client-protocol` (Zed) — ACP-типы и JSON-RPC-обвязка.
- `rmcp` — MCP SDK.
- HTTP/SSE для OpenAI-compatible — лёгкий клиент поверх `reqwest` + `eventsource-stream` (или `reqwest-eventsource`), не тянуть тяжёлые SDK.
- `tokio` + `serde`/`serde_json` — рантайм и конфиги.
- `ratatui` + `crossterm` — только в TUI-бинарнике (feature или отдельный bin, чтобы `buran serve` не тащил их).
- `tracing` + `tracing-subscriber` — логи в файл/stderr, не в stdio ACP-канал (!).

**Запрет**: никаких wasmtime/extism в v1; никаких provider-specific SDK (openai, anthropic crates).

## Тестирование

- Юнит-тесты: парсинг config/AGENTS.md/.mcp.json/SKILL.md frontmatter, LLM-цикл на mock-сервере (wiremock), MCP handshake против mock stdio-процесса.
- Интеграционный тест ACP: поднять `buran serve` как child process, прогнать initialize → newSession → prompt с mock-LLM (тестовый OpenAI-compatible сервер на wiremock), проверить стрим `session/update`.
- `cargo clippy -- -D warnings` и `cargo fmt --check` должны проходить. CI: GitHub Actions, rust stable + beta, кэш cargo.

## Стиль кода

- Rust 2021, `rust-toolchain.toml` с pinned stable.
- Ошибки: `thiserror` для библиотечных типов, `anyhow` на границе main/CLI.
- CLI: `clap` с derive. Подкоманды: `serve` (дефолт), `tui`, `doctor`, `mcp list`.
- Комментарии и doc-comments — на английском; коммиты — conventional commits (`feat:`, `fix:`, `docs:`, `chore:`).
- Логи: только в stderr/файл. **Никогда не писать в stdout при `serve`** — stdout занят ACP JSON-RPC.

## Open questions (можно решать самому, пометь ADR-заметками в `docs/adr/`)

1. **Компакция контекста**: стратегия усечения длинных сессий (порог токенов? ручной `/compact`? summary-вызов LLM?). Для MVP достаточно ручного триггера.
2. **Permission mapping**: как ACP `session/request_permission` мапится на confirm/deny для tool-calls MCP (per-server? per-tool? per-session flag?).
3. **Расположение скиллов**: глобальная директория + проектная? Merge-стратегия?
4. **Формат транскрипта сессий** (задел под v1.5, не блокер MVP): canonical JSONL (сообщения, tool-calls + результаты, метаданные модели, точки компакции) — это артефакт для resume/fork в будущем. Можно заложить типы сейчас, хранение — потом.

## Roadmap (для ориентира, MVP = v1)

- **v1**: `serve` + TUI + MCP + AGENTS.md + skills + doctor (этот скоуп).
- **v1.5**: sessions store (canonical JSONL) + `--resume` / `--fork` + ACP `loadSession`. Прецедент: OpenClaw продолжает сессии Claude Code через каталог сессий + fork-session.
- **v2**: субагенты как bundled MCP-сервер `buran-subagents` (spawn/send/wait; родитель — ACP-клиент свежих child-процессов; глубина ≤ 2; результат — финальный текст в tool-result).
- **v3**: CTO-оркестрация поверх воркеров: `ssh host -- buran serve` (stdio-туннель сохраняет ACP-предположение), воркер внутри периметра своей машины (свой fs/свои MCP), координатор получает отчёты, а не креды. A2A (AgentCard, opaque tasks) — только когда воркеров станет много/разнородных.

## Верификация готовности MVP (Definition of Done)

- [ ] `cargo build --release` даёт единый бинарник, работает на свежей машине без рантайм-зависимостей.
- [ ] `buran serve` проходит handshake с `agent-client-protocol`-тестом (или Zed, если под рукой).
- [ ] `buran tui` ведёт сессию против `buran serve` end-to-end: стрим текста, один tool-call через MCP, один approve/deny.
- [ ] `.mcp.json` с одним сервером (например, `npx @modelcontextprotocol/server-filesystem`) подхватывается и тул доступен модели.
- [ ] AGENTS.md проекта инжектится в системный промпт.
- [ ] SKILL.md-скилл из директории скиллов виден по описанию, тело подгружается.
- [ ] Doctor без настроенных MCP-серверов предлагает ACT (конфиг-строка, не код).
- [ ] Клиент без fs-капабилити не валит сервер (fallbackbundled fs-сервер по конфигу).
- [ ] CI зелёный: fmt, clippy, тесты.

## Не делать

- Не добавлять встроенные tool-имплементации в бинарник (кроме fallback bundled fs-сервера из п.8 Graceful degradation — он тоже MCP-сервер, просто поставляется в комплекте).
- Не вводить per-provider логику в код (только конфиг).
- Не тащить wasmtime/extism/тяжёлые SDK.
- Не писать в stdout из `serve`.
- Не блокировать stdio-потоки sync I/O в tokio-контексте (использовать `tokio::io` или spawn_blocking для child-process stdin/stdout).

## Репозиторий

- GitHub: `GamePad64/buran` (main, пустой на момент старта).
- Лицензия: MIT (владелец подтвердил ранее для харнесса; добавить файл LICENSE).
- Владелец: @GamePad64 (Александр). Архитектурные вопросы — фиксированы выше; сомнения — оформляй ADR-черновик в `docs/adr/NNNN-title.md` и продолжай по наиболее разумному варианту, помечая открытый вопрос в конце файла.
