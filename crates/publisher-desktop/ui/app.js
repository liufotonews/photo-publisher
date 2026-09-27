// Photo Publisher — primeira UI mínima (vanilla JS, sem framework).
//
// A ponte com o backend é a interface global do Tauri (sem bundler):
// window.__TAURI__.core.invoke. Nenhum estado é persistido (sem
// armazenamento de navegador nem escrita em disco — o caminho do projeto
// vive só em memória, neste módulo).
//
// Os eventos do backend chegam pelo canal único "publisher://event" e são
// apresentados na área "Atividade" (também sem persistência).

const tauri = window.__TAURI__ ? window.__TAURI__.core : null;
const tauriEvents = window.__TAURI__ ? window.__TAURI__.event : null;

const state = {
  projectPath: "",
  validated: false,
  preflightReady: false,
  dryRunReady: false,
  busy: false,
  needsRecovery: false,
};

// Generation counter against ABA races: a stale path may reappear (A→B→A),
// so path-equality alone cannot decide if an async result is still current.
// Any accepted answer must carry the generation captured before the await
// and the same captured path.
let operationGeneration = 0;

/// Starts a new UI operation and returns its generation; changing the project
/// path also advances the generation so prior in-flight operations become
/// stale even if the user types the same path again.
function beginOperation() {
  operationGeneration += 1;
  return operationGeneration;
}

/// An operation is current only if the generation and the path both still
/// match what the operation captured.
function isCurrentOperation(generation, path) {
  return operationGeneration === generation && path === currentPath();
}

const el = (id) => document.getElementById(id);

function currentPath() {
  return el("project-path").value.trim();
}

function canPublish() {
  return (
    !state.busy &&
    state.validated &&
    state.preflightReady &&
    state.dryRunReady &&
    state.projectPath !== "" &&
    currentPath() === state.projectPath
  );
}

// O Dry Run exige um Preflight bem-sucedido do projeto atualmente validado;
// o Preflight exige uma validação corrente. Ambos seguem o mesmo caminho.
function canPreflight() {
  return (
    !state.busy &&
    state.validated &&
    state.projectPath !== "" &&
    currentPath() === state.projectPath
  );
}

function canDryRun() {
  return (
    !state.busy &&
    state.validated &&
    state.preflightReady &&
    state.projectPath !== "" &&
    currentPath() === state.projectPath
  );
}

// Recovery is reachable only after a publish result says "needs_recovery"
// for the path currently displayed and nothing else is running.
function canRecover() {
  return (
    !state.busy &&
    state.needsRecovery &&
    state.projectPath !== "" &&
    currentPath() === state.projectPath
  );
}

function refreshButtons() {
  el("validate-button").disabled = state.busy;
  el("config-validate-button").disabled = state.busy;
  el("preflight-button").disabled = !canPreflight();
  el("dry-run-button").disabled = !canDryRun();
  el("publish-button").disabled = !canPublish();
  el("recover-button").disabled = !canRecover();
  // O passo ativo da sequência visual acompanha o estado corrente.
  setPipelineStep();
}

function setPipelineStep() {
  const steps = [
    "step-project",
    "step-validate",
    "step-preflight",
    "step-dryrun",
    "step-publish",
  ];
  for (const id of steps) {
    el(id).classList.remove("current");
  }
  const label = el("global-status").textContent;
  let active = "step-project";
  if (state.needsRecovery) {
    active = "step-publish"; // recovery renders inside its own section
  } else if (label.includes("recuperar") || label.includes("Recupera")) {
    active = "step-publish";
  } else if (label.includes("publica")) {
    active = "step-publish";
  } else if (label.includes("Preflight")) {
    active = "step-preflight";
  } else if (label.includes("Dry Run") || label.includes("dry")) {
    active = "step-dryrun";
  } else if (label.includes("Valid") || label.includes("valid")) {
    active = "step-validate";
  } else if (state.validated && !state.preflightReady) {
    active = "step-preflight";
  } else if (state.preflightReady && !state.dryRunReady) {
    active = "step-dryrun";
  } else if (state.dryRunReady) {
    active = "step-dryrun";
  }
  el(active).classList.add("current");
}

function setBusy(busy, label) {
  state.busy = busy;
  const chip = el("global-status");
  chip.textContent = label;
  // Tonalidade puramente visual; o texto continua sendo a mensagem produzida
  // pelos handlers existentes (nenhuma regra alterada).
  chip.className = "status-chip";
  if (busy) {
    chip.classList.add("busy");
  } else if (label.includes("Falha") || label.includes("Erro")) {
    chip.classList.add("error");
  } else if (label === "Pronto") {
    chip.classList.add("ok");
  } else {
    chip.classList.add("done");
  }
  refreshButtons();
}

function hide(id) {
  el(id).hidden = true;
}

function showValidation(outcome) {
  el("project-result").hidden = false;
  el("project-result-title").textContent = "Projeto válido";
  el("project-status").textContent = "";
  el("project-name").textContent = outcome.project.name;
  el("project-id").textContent = outcome.project.id;
  el("project-kind").textContent = outcome.project.kind;
}

function showValidationError(error) {
  el("project-result").hidden = false;
  el("project-result-title").textContent = "Validação falhou";
  el("project-status").textContent =
    error && error.message ? error.message : "Erro desconhecido.";
  // Limpa o resumo anterior: o erro fica visualmente separado.
  el("project-name").textContent = "";
  el("project-id").textContent = "";
  el("project-kind").textContent = "";
}

// ---------------------------------------------------------------------------
// Configuration validation (Phase 7-D)
//
// Diagnóstico puro da configuração declarada: apresenta os issues que o
// backend reporta e NADA mais. Não altera a máquina de estados da
// publicação (validated/dryRunReady/needsRecovery/projectPath ficam
// intactos) e nunca chama publish/dry-run/recover/create.
// ---------------------------------------------------------------------------

function showConfiguration(outcome) {
  el("configuration-result").hidden = false;
  el("configuration-result-title").textContent = "Configuração";
  const list = el("config-issues");
  // Limpa via textContent: nenhum HTML é gerado a partir de dados externos.
  list.textContent = "";
  if (outcome.valid) {
    el("config-status").textContent = "✓ Configuração coerente para publicação.";
    return;
  }
  el("config-status").textContent = "⚠ A configuração requer correções.";
  for (const issue of outcome.issues) {
    const item = document.createElement("li");
    item.textContent = `${issue.field}: ${issue.message}`;
    list.appendChild(item);
  }
}

function showConfigurationError(error) {
  el("configuration-result").hidden = false;
  el("configuration-result-title").textContent = "Configuração";
  el("config-status").textContent =
    error && error.message ? error.message : "Erro desconhecido.";
  el("config-issues").textContent = "";
}

// ---------------------------------------------------------------------------
// Preflight (Phase 7-F)
//
// Verificação de pré-condições para uma tentativa de publicação: usa o
// comando dedicado (que só lê bits de credencial, configuração e a pasta de
// origem) e atualiza o estado `preflightReady`. É um passo explícito entre
// Validar e Dry Run — nunca publica, nunca planeia, nunca executa nada
// remoto.
// ---------------------------------------------------------------------------

function showPreflight(outcome) {
  el("preflight-result").hidden = false;
  el("preflight-result-title").textContent = "Preflight";
  const list = el("preflight-issues");
  // Limpa via textContent: nenhum HTML é gerado a partir de dados externos.
  list.textContent = "";
  if (outcome.ready) {
    el("preflight-status").textContent =
      outcome.schema_version === 2
        ? "✓ Pronto para publicação integrada."
        : "✓ Pronto para publicação local.";
    return;
  }
  el("preflight-status").textContent =
    "⚠ Não é possível continuar até corrigir o assinalado.";
  for (const issue of outcome.issues) {
    const item = document.createElement("li");
    item.textContent = `${issue.field}: ${issue.message}`;
    list.appendChild(item);
  }
}

function showPreflightError(error) {
  el("preflight-result").hidden = false;
  el("preflight-result-title").textContent = "Preflight";
  el("preflight-status").textContent =
    error && error.message ? error.message : "Erro desconhecido.";
  el("preflight-issues").textContent = "";
}

async function onPreflight() {
  if (state.busy || !state.validated) return;
  const projectPath = state.projectPath;
  if (!projectPath || currentPath() !== projectPath) return;
  const generation = beginOperation();
  state.preflightReady = false;
  // Um novo Preflight é uma nova verificação de pré-condições: o Dry Run
  // anterior deixa de representar o estado atual e tem de ser repetido.
  state.dryRunReady = false;
  hide("preflight-result");
  hide("dry-run-result");
  setBusy(true, "A executar Preflight…");
  try {
    const outcome = await tauri.invoke("preflight_project", { projectPath });
    // Mesma disciplina de stale/ABA dos outros handlers: uma resposta antiga
    // não toca o estado atual.
    if (currentPath() !== projectPath) {
      return;
    }
    if (state.projectPath !== projectPath) {
      return;
    }
    if (!isCurrentOperation(generation, projectPath)) {
      return;
    }
    state.preflightReady = Boolean(outcome.ready);
    showPreflight(outcome);
    setBusy(
      false,
      outcome.ready ? "Preflight concluído." : "Preflight requer correções."
    );
    el("dry-run-button").disabled = !canDryRun();
  } catch (error) {
    if (currentPath() !== projectPath) {
      return;
    }
    if (!isCurrentOperation(generation, projectPath)) {
      return;
    }
    state.preflightReady = false;
    showPreflightError(error);
    setBusy(false, "Falha no Preflight.");
  }
}

async function onValidateConfiguration() {
  if (state.busy) return;
  const projectPath = currentPath();
  const generation = beginOperation();
  hide("configuration-result");
  setBusy(true, "A validar configuração…");
  try {
    const outcome = await tauri.invoke("validate_project_configuration", {
      projectPath,
    });
    // Mesma disciplina de stale/ABA dos outros handlers: uma resposta
    // antiga não toca o estado atual.
    if (currentPath() !== projectPath) {
      return;
    }
    if (!isCurrentOperation(generation, projectPath)) {
      return;
    }
    showConfiguration(outcome);
    setBusy(
      false,
      outcome.valid
        ? "Configuração válida."
        : "Configuração requer correções."
    );
  } catch (error) {
    if (currentPath() !== projectPath) {
      return;
    }
    if (!isCurrentOperation(generation, projectPath)) {
      return;
    }
    showConfigurationError(error);
    setBusy(false, "Erro na validação da configuração.");
  }
}

function showDryRun(outcome) {
  el("dry-run-result").hidden = false;
  el("dry-run-result-title").textContent = "Dry Run";
  el("dry-run-status").textContent = "";
  el("dry-run-generation").textContent = outcome.generation;
  el("dry-run-storage").textContent = outcome.storage_operations;
  el("dry-run-repository").textContent = outcome.repository_operations;
  el("dry-run-hosting").textContent = outcome.hosting_operations;
  el("dry-run-reconciliation").textContent = outcome.reconciliation_requirements;
}

function showDryRunError(error) {
  el("dry-run-result").hidden = false;
  el("dry-run-result-title").textContent = "Dry Run";
  el("dry-run-status").textContent =
    "Não foi possível executar o Dry Run: " +
    (error && error.message ? error.message : "Erro desconhecido.");
  el("dry-run-generation").textContent = "";
  el("dry-run-repository").textContent = "";
  el("dry-run-storage").textContent = "";
  el("dry-run-hosting").textContent = "";
  el("dry-run-reconciliation").textContent = "";
}

const PUBLISH_OUTCOME_MESSAGES = {
  published: {
    title: "Publicado",
    text: "A publicação foi concluída.",
  },
  no_change: {
    title: "Sem alterações",
    text: "O projeto já está sincronizado.",
  },
  blocked: {
    title: "Publicação bloqueada",
    text: "A publicação não foi executada porque existem pendências que precisam ser resolvidas.",
  },
  needs_recovery: {
    title: "Recuperação necessária",
    text: "A publicação anterior requer recuperação antes de uma nova publicação.",
  },
};

function showPublish(outcome) {
  el("publish-result").hidden = false;
  const message = PUBLISH_OUTCOME_MESSAGES[outcome.outcome];
  // "needs_recovery" is the only publish outcome that unlocks the explicit
  // Recovery action; "blocked" and the others do not.
  state.needsRecovery = outcome.outcome === "needs_recovery";
  el("recover-section").hidden = !state.needsRecovery;
  el("publish-result-title").textContent = message ? message.title : "Publicação";
  el("publish-status").textContent = message ? message.text : "";
  refreshButtons();
}

function showPublishError(error) {
  el("publish-result").hidden = false;
  el("publish-result-title").textContent = "Publicação falhou";
  el("publish-status").textContent =
    error && error.message ? error.message : "Erro desconhecido.";
}

function showRecover(outcome) {
  el("recover-result").hidden = false;
  if (outcome.recovered) {
    el("recover-result-title").textContent = "Recuperação concluída";
    el("recover-status").textContent = "A publicação anterior foi restaurada.";
  } else {
    el("recover-result-title").textContent = "Nada a recuperar";
    el("recover-status").textContent = "Não havia publicação pendente.";
  }
}

function showRecoverError(error) {
  el("recover-result").hidden = false;
  el("recover-result-title").textContent = "Recuperação falhou";
  el("recover-status").textContent =
    error && error.message ? error.message : "Erro desconhecido.";
}

// ---------------------------------------------------------------------------
// Event bridge (publisher://event)
//
// The listener translates each DesktopEvent into a friendly message and shows
// it in the activity area. It is presentation only: no state machine, no
// queue, no persistence, no per-event backend calls.
// ---------------------------------------------------------------------------

const WORKFLOW_STEP_MESSAGES = {
  load_project: {
    entered: "Carregando projeto…",
    left_ok: "Projeto carregado.",
    left_failed: "Falha ao carregar o projeto.",
  },
  validate_project: {
    entered: "Validando projeto…",
    left_ok: "Projeto validado.",
    left_failed: "Falha na validação do projeto.",
  },
  inspect_project: {
    entered: "Inspecionando publicação…",
    left_ok: "Inspeção concluída.",
    left_failed: "Falha na inspeção.",
  },
  recover_publication: {
    entered: "Verificando recuperação…",
    left_ok: "Recuperação verificada.",
    left_failed: "Falha na recuperação.",
  },
  preflight: {
    entered: "Preparando publicação…",
    left_ok: "Preparação concluída.",
    left_failed: "Falha na preparação.",
  },
  local_publication: {
    entered: "Preparando publicação local…",
    left_ok: "Publicação local pronta.",
    left_failed: "Falha na publicação local.",
  },
  build_plan: {
    entered: "Calculando alterações…",
    left_ok: "Alterações calculadas.",
    left_failed: "Falha ao calcular alterações.",
  },
  publish_integrate: {
    entered: "Publicando…",
    left_ok: "Publicação concluída.",
    left_failed: "Falha na publicação.",
  },
  dry_run: {
    entered: "Simulando publicação…",
    left_ok: "Simulação concluída.",
    left_failed: "Falha na simulação.",
  },
};

const OPERATION_MESSAGES = {
  storage_put_started: "Enviando arquivo…",
  storage_put_finished: "Arquivo enviado.",
  storage_put_failed: "Falha no envio do arquivo.",
  storage_delete_started: "Removendo arquivo…",
  storage_delete_finished: "Arquivo removido.",
  storage_delete_failed: "Falha ao remover arquivo.",
  repository_batch_started: "Atualizando galeria…",
  repository_batch_finished: "Galeria atualizada.",
  repository_batch_failed: "Falha ao atualizar a galeria.",
  hosting_publish_started: "Publicando site…",
  hosting_publish_finished: "Site publicado.",
  hosting_publish_failed: "Falha ao publicar o site.",
};

// Maximum rendered entries — kept bounded so a single long run cannot grow
// the list forever. Presentation-only trim.
const ACTIVITY_LIMIT = 100;

function describePublisherEvent(event) {
  if (!event || typeof event.type !== "string") {
    return null;
  }
  if (event.type === "workflow" && event.data) {
    if (event.data.lifecycle === "finished") return { text: "Operação concluída." };
    if (event.data.lifecycle === "failed") return { text: "Operação falhou.", failed: true };
    const step = WORKFLOW_STEP_MESSAGES[event.data.step];
    const text = step ? step[event.data.lifecycle] : null;
    return text
      ? { text, failed: event.data.lifecycle === "left_failed" }
      : null;
  }
  if (event.type === "operation" && event.data) {
    const text = OPERATION_MESSAGES[event.data.event];
    return text ? { text, failed: event.data.event.endsWith("_failed") } : null;
  }
  return null;
}

function addActivity(message, failed) {
  const list = el("activity-list");
  const item = document.createElement("li");
  item.textContent = message;
  // Tonalidade da entrada é derivada do texto produzido pela camada de
  // apresentação — nunca de uma decisão nova.
  if (failed) {
    item.classList.add("failed");
  } else if (message.endsWith("…")) {
    item.classList.add("in-progress");
  } else {
    item.classList.add("done");
  }
  list.appendChild(item);
  while (list.children.length > ACTIVITY_LIMIT) {
    list.removeChild(list.firstChild);
  }
  item.scrollIntoView(false);
}

async function installPublisherEventListener() {
  if (!tauriEvents) return;
  await tauriEvents.listen("publisher://event", (message) => {
    const described = describePublisherEvent(message.payload);
    if (described) {
      addActivity(described.text, described.failed);
    }
  });
}

async function onValidate() {
  if (state.busy) return;
  const projectPath = currentPath();
  const generation = beginOperation();
  hide("project-result");
  hide("dry-run-result");
  hide("publish-result");
  state.validated = false;
  state.dryRunReady = false;
  // Uma nova validação invalida sempre o Preflight anterior: a sequência
  // volta a exigir Validar → Preflight → Dry Run para este caminho.
  state.preflightReady = false;
  hide("preflight-result");
  el("preflight-button").disabled = true;
  el("dry-run-button").disabled = true;
  el("publish-button").disabled = true;
  state.projectPath = projectPath;
  state.needsRecovery = false;
  hide("recover-result");
  hide("recover-section");
  setBusy(true, "A validar…");
  try {
    const outcome = await tauri.invoke("validate_project", { projectPath });
    // Guard against a stale async result: if the user edited the path while
    // the backend validated, this answer describes a different project and
    // must be ignored entirely — a stale answer never touches current state.
    if (currentPath() !== projectPath) {
      return;
    }
    // ABA: o caminho pode ter voltado ao mesmo valor; a geração da operação
    // é a identidade que impede reutilizar um resultado antigo.
    if (!isCurrentOperation(generation, projectPath)) {
      return;
    }
    state.validated = Boolean(outcome.valid);
    showValidation(outcome);
    setBusy(false, "Projeto válido.");
    el("preflight-button").disabled = !canPreflight();
    el("dry-run-button").disabled = !canDryRun();
    el("publish-button").disabled = !canPublish();
  } catch (error) {
    if (currentPath() !== projectPath) {
      return;
    }
    if (!isCurrentOperation(generation, projectPath)) {
      return;
    }
    state.validated = false;
    showValidationError(error);
    setBusy(false, "Erro na validação.");
  }
}

async function onDryRun() {
  if (state.busy || !state.validated) return;
  const projectPath = state.projectPath;
  if (!projectPath || currentPath() !== projectPath) return;
  const generation = beginOperation();
  state.needsRecovery = false;
  hide("recover-result");
  hide("recover-section");
  hide("dry-run-result");
  hide("publish-result");
  state.dryRunReady = false;
  setBusy(true, "A executar Dry Run…");
  try {
    const outcome = await tauri.invoke("dry_run_project", { projectPath });
    // Same staleness guard as validation: an answer about a superseded path
    // is ignored entirely — a stale answer never touches current state.
    if (currentPath() !== projectPath) {
      return;
    }
    if (state.projectPath !== projectPath) {
      return;
    }
    // ABA: a geração é a identidade — um Dry Run antigo nunca reabilita
    // Publicar só porque o caminho voltou a coincidir.
    if (!isCurrentOperation(generation, projectPath)) {
      return;
    }
    state.dryRunReady = true;
    showDryRun(outcome);
    setBusy(false, "Dry Run concluído.");
    el("publish-button").disabled = !canPublish();
  } catch (error) {
    if (currentPath() !== projectPath) {
      return;
    }
    if (!isCurrentOperation(generation, projectPath)) {
      return;
    }
    showDryRunError(error);
    setBusy(false, "Falha no Dry Run.");
  }
}

async function onPublish() {
  if (!canPublish()) return;
  const projectPath = state.projectPath;
  const generation = beginOperation();
  hide("publish-result");
  setBusy(true, "A publicar…");
  try {
    const outcome = await tauri.invoke("publish_project", { projectPath });
    // A publish result is never cancelled; it is only refused as UI state
    // when the user already moved on to a different path — a stale answer
    // never touches current state.
    if (currentPath() !== projectPath) {
      return;
    }
    // ABA: the generation guards against an answer produced for a previous
    // session over the same path.
    if (!isCurrentOperation(generation, projectPath)) {
      return;
    }
    state.dryRunReady = false;
    showPublish(outcome);
    setBusy(false, "Publicação atualizada.");
  } catch (error) {
    if (currentPath() !== projectPath) {
      return;
    }
    if (!isCurrentOperation(generation, projectPath)) {
      return;
    }
    showPublishError(error);
    setBusy(false, "Falha na publicação.");
  }
}

async function onRecover() {
  if (!canRecover()) return;
  const projectPath = state.projectPath;
  const generation = beginOperation();
  state.needsRecovery = false;
  state.dryRunReady = false;
  hide("recover-result");
  hide("publish-result");
  hide("dry-run-result");
  setBusy(true, "A recuperar…");
  try {
    const outcome = await tauri.invoke("recover_project", { projectPath });
    if (currentPath() !== projectPath) {
      return;
    }
    if (!isCurrentOperation(generation, projectPath)) {
      return;
    }
    showRecover(outcome);
    setBusy(false, "Recuperação verificada. Valide e execute Dry Run novamente.");
  } catch (error) {
    if (currentPath() !== projectPath) {
      return;
    }
    if (!isCurrentOperation(generation, projectPath)) {
      return;
    }
    showRecoverError(error);
    setBusy(false, "Falha na recuperação.");
  }
}

// Uma alteração ao caminho invalida qualquer validação anterior: a UI deixa
// de ser "validada", o Dry Run fica desabilitado e os resultados anteriores
// são removidos. Nada é executado e nada de Rust é chamado.
function onProjectPathChanged() {
  beginOperation(); // invalidates any in-flight operation's generation too
  state.validated = false;
  state.needsRecovery = false;
  hide("recover-result");
  hide("recover-section");
  state.preflightReady = false;
  state.dryRunReady = false;
  state.projectPath = "";
  el("preflight-button").disabled = true;
  el("dry-run-button").disabled = true;
  el("publish-button").disabled = true;
  hide("project-result");
  hide("configuration-result");
  hide("preflight-result");
  hide("dry-run-result");
  hide("publish-result");
  // The superseded operation is no longer current: the UI frees its busy
  // state here — never from the stale result, which cannot cancel anything
  // already started. setBusy only recalculates buttons from cleared state.
  setBusy(false, "Pronto");
}

// ---------------------------------------------------------------------------
// Credential UX (Phase 7-E)
//
// As credenciais são geridas apenas pelos três commands dedicados. O valor
// é lido do campo no momento do clique, enviado uma única vez ao backend e
// limpo de imediato — nunca fica em estado, nunca é escrito no DOM, nunca é
// copiado para a área de transferência, nunca é persistido na página. O
// estado do fluxo de publicação (validated/dryRunReady/busy/generation)
// não é tocado por este bloco.
// ---------------------------------------------------------------------------

const credentialsState = {
  busy: false,
  generation: 0,
};

function credentialRows() {
  return Array.from(
    document.querySelectorAll("#credentials-section .credential-value")
  );
}

function showCredentialNote(message) {
  el("credential-note").textContent = message;
}

function markCredentialRow(row, configured) {
  row.querySelector(".credential-state").textContent = configured
    ? "● Configurada"
    : "○ Não configurada";
}

function renderCredentialStatus(statuses) {
  const byName = new Map(statuses.map((entry) => [entry.name, entry]));
  for (const input of credentialRows()) {
    const entry = byName.get(input.dataset.credentialName);
    markCredentialRow(input.closest(".credential-row"), Boolean(entry && entry.configured));
  }
}

async function refreshCredentialStatus() {
  if (!tauri) return;
  const generation = (credentialsState.generation += 1);
  try {
    const statuses = await tauri.invoke("get_credential_status");
    if (generation !== credentialsState.generation) {
      return;
    }
    renderCredentialStatus(statuses);
  } catch (error) {
    if (generation !== credentialsState.generation) {
      return;
    }
    showCredentialNote(error && error.message ? error.message : "Erro desconhecido.");
  }
}

async function onCredentialAction(button) {
  if (credentialsState.busy || !tauri) return;
  const row = button.closest(".credential-row");
  const input = row.querySelector(".credential-value");
  const name = input.dataset.credentialName;
  const action = button.dataset.credentialAction;
  const generation = (credentialsState.generation += 1);

  if (action === "remove") {
    if (!window.confirm("Remover esta credencial?")) {
      return;
    }
    credentialsState.busy = true;
    try {
      await tauri.invoke("delete_credential", { name });
      if (generation !== credentialsState.generation) {
        return;
      }
      markCredentialRow(row, false);
      showCredentialNote("Credencial removida.");
    } catch (error) {
      if (generation !== credentialsState.generation) {
        return;
      }
      showCredentialNote(error && error.message ? error.message : "Erro desconhecido.");
    } finally {
      credentialsState.busy = false;
    }
    return;
  }

  const value = input.value;
  // O valor sai do campo imediatamente: nunca permanece no DOM após o clique.
  input.value = "";
  if (value.trim() === "") {
    showCredentialNote("Indique o valor a guardar.");
    return;
  }
  credentialsState.busy = true;
  try {
    await tauri.invoke("set_credential", { name, value });
    if (generation !== credentialsState.generation) {
      return;
    }
    markCredentialRow(row, true);
    showCredentialNote("Credencial guardada.");
  } catch (error) {
    if (generation !== credentialsState.generation) {
      return;
    }
    showCredentialNote(error && error.message ? error.message : "Erro desconhecido.");
  } finally {
    credentialsState.busy = false;
  }
}

function setCredentialsMode(active) {
  el("tab-credentials").classList.toggle("active", active);
  if (active) {
    // Reutiliza o modo do wizard para esconder todo o fluxo de publicação…
    setCreateMode(true);
    // …mas a secção visível aqui é a de credenciais, não o wizard.
    el("create-flow").hidden = true;
    el("tab-create").classList.remove("active");
    el("credentials-section").hidden = false;
    // Respostas de operações anteriores ficam obsoletas ao trocar de modo.
    credentialsState.generation += 1;
    refreshCredentialStatus();
  } else {
    credentialsState.generation += 1;
    el("credentials-section").hidden = true;
    setCreateMode(false);
  }
}

// ---------------------------------------------------------------------------
// Project setup wizard (Phase 7-C)
//
// The wizard only collects data and produces one `project.json` via the
// desktop command; publishing, dry-run and recovery are untouched. The
// wizard keeps its own independent state and never mutates the publication
// workflow state.
// ---------------------------------------------------------------------------

const wizardState = {
  busy: false,
  generation: 0,
};

function wizardValue(id) {
  return el(id).value.trim();
}

function wizardOptional(id) {
  const v = wizardValue(id);
  return v === "" ? null : v;
}

function showCreated(outcome) {
  el("create-result").hidden = false;
  el("create-result-title").textContent = "Projeto criado";
  el("create-status").textContent =
    `${outcome.project_name} (${outcome.project_id}) — gravado em ${outcome.project_path}`;
}

function showCreateError(message) {
  el("create-result").hidden = false;
  el("create-result-title").textContent = "Criação falhou";
  el("create-status").textContent = message;
}

async function onCreateProject() {
  if (wizardState.busy) return;
  // Presentation-only checks: schema remains the authority.
  const requiredFields = [
    "wizard-project-id",
    "wizard-project-name",
    "wizard-gallery-template",
    "wizard-gallery-title",
    "wizard-project-path",
    "wizard-repository-provider",
    "wizard-repository-name",
    "wizard-hosting-provider",
    "wizard-preview-provider",
    "wizard-hires-provider",
  ];
  for (const id of requiredFields) {
    if (wizardValue(id) === "") {
      showCreateError("Preencha todos os campos obrigatórios.");
      return;
    }
  }

  const generation = (wizardState.generation += 1);
  el("create-submit").disabled = true;
  el("create-cancel").disabled = true;
  wizardState.busy = true;
  hide("create-result");
  try {
    const setup = {
      schemaVersion: 2,
      project: {
        id: wizardValue("wizard-project-id"),
        name: wizardValue("wizard-project-name"),
        ...(wizardOptional("wizard-project-client") && { client: wizardOptional("wizard-project-client") }),
        ...(wizardOptional("wizard-project-date") && { date: wizardOptional("wizard-project-date") }),
      },
      gallery: {
        template: wizardValue("wizard-gallery-template"),
        title: wizardValue("wizard-gallery-title"),
        ...(wizardOptional("wizard-gallery-description") && { description: wizardOptional("wizard-gallery-description") }),
        ...(wizardOptional("wizard-gallery-bundle") && { bundlePath: wizardOptional("wizard-gallery-bundle") }),
      },
      source: { type: "folder", path: wizardOptional("wizard-source-path") },
      repository: {
        provider: wizardValue("wizard-repository-provider"),
        repository: wizardValue("wizard-repository-name"),
        ...(wizardOptional("wizard-repository-branch") && { branch: wizardOptional("wizard-repository-branch") }),
      },
      hosting: {
        provider: wizardValue("wizard-hosting-provider"),
        ...(wizardOptional("wizard-hosting-project") && { project: wizardOptional("wizard-hosting-project") }),
        ...(wizardOptional("wizard-hosting-team") && { teamId: wizardOptional("wizard-hosting-team") }),
      },
      storage: {
        preview: {
          provider: wizardValue("wizard-preview-provider"),
          ...(wizardOptional("wizard-preview-prefix") && { prefix: wizardOptional("wizard-preview-prefix") }),
          ...(wizardOptional("wizard-preview-public-url") && { publicBaseUrl: wizardOptional("wizard-preview-public-url") }),
        },
        highResolution: {
          provider: wizardValue("wizard-hires-provider"),
          ...(wizardOptional("wizard-hires-bucket") && { bucket: wizardOptional("wizard-hires-bucket") }),
          ...(wizardOptional("wizard-hires-prefix") && { prefix: wizardOptional("wizard-hires-prefix") }),
          ...(wizardOptional("wizard-hires-account") && { accountId: wizardOptional("wizard-hires-account") }),
          ...(wizardOptional("wizard-hires-public-url") && { publicBaseUrl: wizardOptional("wizard-hires-public-url") }),
        },
      },
      ...(wizardOptional("wizard-domain-url") && { domain: { url: wizardOptional("wizard-domain-url") } }),
    };

    const outcome = await tauri.invoke("create_project_setup", {
      projectPath: wizardValue("wizard-project-path"),
      setup,
    });
    if (generation !== wizardState.generation) {
      return;
    }
    showCreated(outcome);
  } catch (error) {
    if (generation !== wizardState.generation) {
      return;
    }
    showCreateError(error && error.message ? error.message : "Erro desconhecido.");
  } finally {
    wizardState.busy = false;
    el("create-submit").disabled = false;
    el("create-cancel").disabled = false;
  }
}

function setCreateMode(active) {
  el("tab-publish").classList.toggle("active", !active);
  el("tab-create").classList.toggle("active", active);
  // A secção de credenciais nunca se mistura com os outros dois modos.
  el("tab-credentials").classList.remove("active");
  el("credentials-section").hidden = true;
  // Hide every publication-flow piece: pipeline nav, project card, all result
  // cards and the activity log must not leak into the setup screen.
  document.querySelector("nav.pipeline").hidden = active;
  document.querySelector('[aria-labelledby="project-section-title"]').hidden = active;
  el("project-result").hidden = active;
  el("configuration-result").hidden = active;
  el("preflight-result").hidden = active;
  el("dry-run-result").hidden = active;
  el("publish-result").hidden = active;
  el("recover-section").hidden = active;
  el("activity").hidden = active;
  el("create-flow").hidden = !active;
  if (active) {
    hide("create-result");
  }
}

async function main() {
  el("validate-button").addEventListener("click", onValidate);
  el("config-validate-button").addEventListener("click", onValidateConfiguration);
  el("preflight-button").addEventListener("click", onPreflight);
  el("dry-run-button").addEventListener("click", onDryRun);
  el("publish-button").addEventListener("click", onPublish);
  el("recover-button").addEventListener("click", onRecover);
  el("project-path").addEventListener("input", onProjectPathChanged);
  el("tab-publish").addEventListener("click", () => setCredentialsMode(false));
  el("tab-create").addEventListener("click", () => setCreateMode(true));
  el("tab-credentials").addEventListener("click", () => setCredentialsMode(true));
  for (const button of document.querySelectorAll("#credentials-section [data-credential-action]")) {
    button.addEventListener("click", () => onCredentialAction(button));
  }
  el("create-cancel").addEventListener("click", () => setCreateMode(false));
  el("create-submit").addEventListener("click", onCreateProject);
  // The event listener is installed once, at startup.
  installPublisherEventListener();
  if (tauri) {
    try {
      const info = await tauri.invoke("get_app_info");
      el("app-version").textContent = `${info.name} ${info.version}`;
    } catch {
      // A versão é apenas informativa; ausência nunca bloqueia a UI.
    }
  } else {
    el("app-version").textContent = "";
  }
  setBusy(false, "Pronto");
}

document.addEventListener("DOMContentLoaded", main);
