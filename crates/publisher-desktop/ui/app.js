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
  dryRunReady: false,
  busy: false,
};

const el = (id) => document.getElementById(id);

function currentPath() {
  return el("project-path").value.trim();
}

function canPublish() {
  return (
    !state.busy &&
    state.validated &&
    state.dryRunReady &&
    state.projectPath !== "" &&
    currentPath() === state.projectPath
  );
}

function setBusy(busy, label) {
  state.busy = busy;
  el("validate-button").disabled = busy;
  el("dry-run-button").disabled = busy || !state.validated;
  el("publish-button").disabled = !canPublish();
  el("global-status").textContent = label;
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
  el("publish-result-title").textContent = message ? message.title : "Publicação";
  el("publish-status").textContent = message ? message.text : "";
}

function showPublishError(error) {
  el("publish-result").hidden = false;
  el("publish-result-title").textContent = "Publicação falhou";
  el("publish-status").textContent =
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
  if (failed) item.classList.add("failed");
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
  hide("project-result");
  hide("dry-run-result");
  hide("publish-result");
  state.validated = false;
  state.dryRunReady = false;
  el("dry-run-button").disabled = true;
  el("publish-button").disabled = true;
  state.projectPath = projectPath;
  setBusy(true, "A validar…");
  try {
    const outcome = await tauri.invoke("validate_project", { projectPath });
    // Guard against a stale async result: if the user edited the path while
    // the backend validated, this answer describes a different project and
    // must be ignored entirely (no re-validation, no Dry Run reactivation).
    if (currentPath() !== projectPath) {
      setBusy(false, "Pronto");
      return;
    }
    state.validated = Boolean(outcome.valid);
    showValidation(outcome);
    setBusy(false, "Projeto válido.");
    el("dry-run-button").disabled = !state.validated;
    el("publish-button").disabled = !canPublish();
  } catch (error) {
    if (currentPath() !== projectPath) {
      setBusy(false, "Pronto");
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
  hide("dry-run-result");
  hide("publish-result");
  state.dryRunReady = false;
  setBusy(true, "A executar Dry Run…");
  try {
    const outcome = await tauri.invoke("dry_run_project", { projectPath });
    // Same staleness guard as validation: an answer about a superseded path
    // is ignored entirely.
    if (currentPath() !== projectPath) {
      setBusy(false, "Pronto");
      return;
    }
    if (state.projectPath !== projectPath) {
      setBusy(false, "Pronto");
      return;
    }
    state.dryRunReady = true;
    showDryRun(outcome);
    setBusy(false, "Dry Run concluído.");
    el("publish-button").disabled = !canPublish();
  } catch (error) {
    if (currentPath() !== projectPath) {
      setBusy(false, "Pronto");
      return;
    }
    showDryRunError(error);
    setBusy(false, "Falha no Dry Run.");
  }
}

async function onPublish() {
  if (!canPublish()) return;
  const projectPath = state.projectPath;
  hide("publish-result");
  setBusy(true, "A publicar…");
  try {
    const outcome = await tauri.invoke("publish_project", { projectPath });
    // A publish result is never cancelled; it is only refused as UI state
    // when the user already moved on to a different path.
    if (currentPath() !== projectPath) {
      setBusy(false, "Pronto");
      return;
    }
    state.dryRunReady = false;
    el("publish-button").disabled = true;
    showPublish(outcome);
    setBusy(false, canPublish() ? "Dry Run concluído." : "Publicação concluída.");
  } catch (error) {
    if (currentPath() !== projectPath) {
      setBusy(false, "Pronto");
      return;
    }
    showPublishError(error);
    setBusy(false, "Falha na publicação.");
  }
}

// Uma alteração ao caminho invalida qualquer validação anterior: a UI deixa
// de ser "validada", o Dry Run fica desabilitado e os resultados anteriores
// são removidos. Nada é executado e nada de Rust é chamado.
function onProjectPathChanged() {
  state.validated = false;
  state.dryRunReady = false;
  state.projectPath = "";
  el("dry-run-button").disabled = true;
  el("publish-button").disabled = true;
  hide("project-result");
  hide("dry-run-result");
  hide("publish-result");
  if (!state.busy) {
    el("global-status").textContent = "Pronto";
  }
}

async function main() {
  el("validate-button").addEventListener("click", onValidate);
  el("dry-run-button").addEventListener("click", onDryRun);
  el("publish-button").addEventListener("click", onPublish);
  el("project-path").addEventListener("input", onProjectPathChanged);
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
