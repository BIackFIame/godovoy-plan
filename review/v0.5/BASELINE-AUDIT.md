# Security Review: godovoy-plan

## Scope

Статический аудит снимка приложения 0.4 перед подготовкой 0.5.

- Scan mode: repository
- Target kind: directory\_snapshot
- Target ID: target\_sha256\_399b6c16ba267b95150a11ccebc6a0f4f9769c18efc6610ffdc4cd9cd7bccfeb
- Snapshot digest: codex-security-snapshot/v1:sha256:bc954227412bdc43a2507862cfb0bc57ec3938da23ea37d354d4c1607873a45f
- Included paths: .
- Excluded paths: none

Limitations and exclusions:
- Оценка собственных исходников, тестов, конфигураций и документации. Независимые проверки читали локальный снимок.
- Измерения нагрузки и запуск тестовых бинарников относятся к отдельной проверке 0.5. Проверка Windows здесь не выполнялась.

### Scan Summary

| Field | Value |
| --- | --- |
| Scan outcome | completed |
| Reportable findings | 4 |
| Severity mix | medium: 2, low: 2 |
| Confidence mix | high: 4 |
| Validation mode | static |

Canonical artifacts: `scan-manifest.json` and `findings.json`. This report is a deterministic projection of those files.

## Threat Model

# «Годовой план» 0.4

Календарь, назначения, дневники, токены API и ключ пространства хранятся в профиле ОС. UI, LAN-обмен и отключаемый loopback HTTP работают внутри desktop-процесса, мост запускается отдельно. 

Нужны независимые границы между внешним HTTPS-источником и мостом, между мостом и профилем, между неподтверждёнными объявлениями LAN и Noise-сессией, а также между авторизованным HTTP-запросом и транзакцией на диске. Сотрудники, которым выдан общий ключ LAN, в 0.4 взаимно доверены. Другие учётные записи ОС на этом же компьютере не считаются участниками пространства. Пароль администратора, захват устройства и цепочка поставки бинарников выходят за границу модели.

Цели: исключить выдачу учётных данных другому локальному процессу, соблюдать отзыв права записи, сохранять атомарность и сходимость офлайн-реплик, ограничивать сеть, память и объём локального профиля.

## Findings

| Finding | Severity | Confidence | Detailed write-up |
| --- | --- | --- | --- |
| [Общий кеш не отделяет чужие объявления](#finding-1) | medium | high | inline below |
| [Мост принимает любой локальный слушатель](#finding-2) | medium | high | inline below |
| [Запись после отзыва токена](#finding-3) | low | high | inline below |
| [Отказ объединения при достижении лимита теряется](#finding-4) | low | high | inline below |

### Confidence Scale

| Label | Meaning |
| --- | --- |
| high | Direct evidence supports the finding with no material unresolved blocker. |
| medium | Evidence supports a plausible issue, but material runtime or reachability proof remains. |
| low | Evidence is incomplete and the item is retained only for explicit follow-up. |

<a id="finding-1"></a>

### [1] Общий кеш не отделяет чужие объявления

| Field | Value |
| --- | --- |
| Severity | medium |
| Confidence | high |
| Confidence rationale | Условия и результат подтверждены исходным кодом и независимым статическим просмотром. |
| Category | Исчерпание ресурсов обнаружения |
| CWE | CWE-770 |
| Affected lines | src/network.rs:393-429, src/network.rs:576-598 |

#### Summary

Ограниченный кеш обнаружения в 0.4 принимает адреса из разных рабочих пространств. При выборе адресов для подключения по коду найденные по сети адреса могут вытеснить встроенный в приглашение адрес из списка попыток.

#### Validation

Условия и результат подтверждены исходным кодом и независимым статическим просмотром. Validation details were not recorded separately.

#### Dataflow

The canonical finding records the affected path at src/network.rs:393-429, src/network.rs:576-598, but no expanded source-to-sink narrative was recorded.

#### Reachability

Reachability was not recorded beyond the canonical finding summary and affected locations.

#### Severity

**Medium** — The scan assigned medium severity; no separate canonical severity rationale was recorded.

Additional runtime or deployment evidence could raise or lower this severity.

#### Remediation

Пробовать ручной и встроенный адреса первыми, не отдавать неподтверждённым адресам все ресурсы обнаружения.

<a id="finding-2"></a>

### [2] Мост принимает любой локальный слушатель

| Field | Value |
| --- | --- |
| Severity | medium |
| Confidence | high |
| Confidence rationale | Условия и результат подтверждены исходным кодом и независимым статическим просмотром. |
| Category | Идентификация API |
| CWE | CWE-346 |
| Affected lines | src/bin/godovoy-bridge.rs:210-238 |

#### Summary

Мост версии 0.4 отправляет токен и данные через HTTP на заранее заданный loopback-порт. Пока приложению этот порт не принадлежит, другой локальный процесс может получить данные и секрет импорта.

#### Validation

Условия и результат подтверждены исходным кодом и независимым статическим просмотром. Validation details were not recorded separately.

#### Dataflow

The canonical finding records the affected path at src/bin/godovoy-bridge.rs:210-238, but no expanded source-to-sink narrative was recorded.

#### Reachability

Reachability was not recorded beyond the canonical finding summary and affected locations.

#### Severity

**Medium** — The scan assigned medium severity; no separate canonical severity rationale was recorded.

Additional runtime or deployment evidence could raise or lower this severity.

#### Remediation

Аутентифицировать локальный сервер до отправки данных и использовать зашифрованный канал.

<a id="finding-3"></a>

### [3] Запись после отзыва токена

| Field | Value |
| --- | --- |
| Severity | low |
| Confidence | high |
| Confidence rationale | Условия и результат подтверждены исходным кодом и независимым статическим просмотром. |
| Category | Отзыв доступа |
| CWE | CWE-367 |
| Affected lines | src/api.rs:129-165, src/api.rs:433-452 |

#### Summary

Для записи токен проверяется до чтения тела запроса. Обработчики 0.4 не повторяют проверку внутри общей блокировки, поэтому уже принятая запись может зафиксироваться после ротации.

#### Validation

Условия и результат подтверждены исходным кодом и независимым статическим просмотром. Validation details were not recorded separately.

#### Dataflow

The canonical finding records the affected path at src/api.rs:129-165, src/api.rs:433-452, but no expanded source-to-sink narrative was recorded.

#### Reachability

Reachability was not recorded beyond the canonical finding summary and affected locations.

#### Severity

**Low** — The scan assigned low severity; no separate canonical severity rationale was recorded.

Additional runtime or deployment evidence could raise or lower this severity.

#### Remediation

Повторно проверить действующий токен под той же блокировкой, что защищает запись.

<a id="finding-4"></a>

### [4] Отказ объединения при достижении лимита теряется

| Field | Value |
| --- | --- |
| Severity | low |
| Confidence | high |
| Confidence rationale | Условия и результат подтверждены исходным кодом и независимым статическим просмотром. |
| Category | Обработка отказа слияния |
| CWE | CWE-755 |
| Affected lines | src/network.rs:301-322 |

#### Summary

Независимо допустимые офлайн-реплики могут вместе превысить лимит. 0.4 откатывает запись, но фоновый обмен не доводит причину до пользователя.

#### Validation

Условия и результат подтверждены исходным кодом и независимым статическим просмотром. Validation details were not recorded separately.

#### Dataflow

The canonical finding records the affected path at src/network.rs:301-322, but no expanded source-to-sink narrative was recorded.

#### Reachability

Reachability was not recorded beyond the canonical finding summary and affected locations.

#### Severity

**Low** — The scan assigned low severity; no separate canonical severity rationale was recorded.

Additional runtime or deployment evidence could raise or lower this severity.

#### Remediation

Показать ошибку слияния и сохранить обе копии. Для расширения ёмкости предусмотреть отдельный цикл архивации с поддержкой старых реплик.
