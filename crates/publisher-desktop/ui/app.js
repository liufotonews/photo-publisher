// Photo Publisher — primeira UI mínima (vanilla JS, sem framework).
//
// A ponte com o backend é a interface global do Tauri (sem bundler):
// window.__TAURI__.core.invoke. Nenhum estado é persistido (sem
// armazenamento de navegador nem escrita em disco — o caminho do projeto
// vive só em memória, neste módulo).

const tauri = window.__TAURI__ ? window.__TAURI__.core : null;

const state = {
  projectPath: "",
  validated: false,
  busy: false,
};

const el = (id) => document.getElementById(id);

function setBusy(busy, label) {
  state.busy = busy;
  el("validate-button").disabled = busy;
  el("dry-run-button").disabled = busy || !state.validated;
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
  el("project-status").textContent = error && error.message
    ? error.message
    : "Erro desconhecido.";
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
  el("dry-run-status").textContent = "Não foi possível executar o Dry Run: " +
    (error && error.message ? error.message : "Erro desconhecido.");
  el("dry-run-generation").textContent = "";
  el("dry-run-repository").textContent = "";
  el("dry-run-storage").textContent = "";
  el("dry-run-hosting").textContent = "";
  el("dry-run-reconciliation").textContent = "";
}

async function onValidate() {
  if (state.busy) return;
  const projectPath = el("project-path").value.trim();
  hide("project-result");
  hide("dry-run-result");
  state.validated = false;
  el("dry-run-button").disabled = true;
  state.projectPath = projectPath;
  setBusy(true, "A validar…");
  try {
    const outcome = await tauri.invoke("validate_project", { projectPath });
    // Guard against a stale async result: if the user edited the path while
    // the backend validated, this answer describes a different project and
    // must be ignored entirely (no re-validation, no Dry Run reactivation).
    if (el("project-path").value.trim() !== projectPath) {
      setBusy(false, "Pronto");
      return;
    }
    state.validated = Boolean(outcome.valid);
    showValidation(outcome);
    setBusy(false, "Projeto válido.");
    el("dry-run-button").disabled = !state.validated;
  } catch (error) {
    if (el("project-path").value.trim() !== projectPath) {
      setBusy(false, "Pronto");
      return;
    }
    state.validated = false;
    showValidationError(error);
    setBusy(false, "Erro na validação.");
  }
}

// Uma alteração ao caminho invalida qualquer validação anterior: a UI deixa
// de ser "validada", o Dry Run fica desabilitado e os resultados anteriores
// são removidos. Nada é executado e nada de Rust é chamado.
function onProjectPathChanged() {
  state.validated = false;
  state.projectPath = "";
  el("dry-run-button").disabled = true;
  hide("project-result");
  hide("dry-run-result");
  if (!state.busy) {
    el("global-status").textContent = "Pronto";
  }
}

async function onDryRun() {
  if (state.busy || !state.validated) return;
  hide("dry-run-result");
  setBusy(true, "A executar Dry Run…");
  try {
    const outcome = await tauri.invoke("dry_run_project", { projectPath: state.projectPath });
    showDryRun(outcome);
    setBusy(false, "Dry Run concluído.");
  } catch (error) {
    showDryRunError(error);
    setBusy(false, "Falha no Dry Run.");
  }
}

async function main() {
  el("validate-button").addEventListener("click", onValidate);
  el("dry-run-button").addEventListener("click", onDryRun);
  el("project-path").addEventListener("input", onProjectPathChanged);
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
