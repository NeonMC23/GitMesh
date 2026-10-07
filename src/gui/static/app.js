// GitMesh interface.
//
// Everything between the clientLogic markers is pure: it takes the JSON model the Rust
// side produces and returns plain values, and it never touches the DOM or the network.
// That region is sliced out of this file by src/gui/asset.rs and executed under Node by
// tests/client.rs, so the code that ships is the code that is tested. Run
// `cargo test --test client` to check it (the test skips when Node is not installed).

/* ==== clientLogic:start ==== */
var GitMesh = (function () {
  'use strict';

  var STATE_LABEL = {
    clean: 'clean',
    changed: 'modified',
    conflicted: 'conflicted',
    unavailable: 'unavailable',
  };

  var CHANGE_LABEL = {
    staged: 'staged',
    staged_and_modified: 'staged + modified',
    modified: 'modified',
    untracked: 'untracked',
    deleted: 'deleted',
    renamed: 'renamed',
    copied: 'copied',
    type_changed: 'type changed',
    conflict: 'conflict',
    ignored: 'ignored',
  };

  // --------------------------------------------------------------- formatting --

  function stateLabel(key) {
    return STATE_LABEL[key] || key || 'unknown';
  }

  function changeLabel(key) {
    return CHANGE_LABEL[key] || key || 'changed';
  }

  function countLabel(count, singular, plural) {
    var word = count === 1 ? singular : (plural || singular + 's');
    return count + ' ' + word;
  }

  // ------------------------------------------------------------------ project --

  function projectSummary(model) {
    var project = model.project || {};
    var counts = project.counts || {};
    var branch = (project.branch || {}).name;
    return {
      opened: !!model.opened,
      name: project.name || '(unknown project)',
      root: project.root || '',
      manifest: project.manifest || '',
      repositoryCount: counts.repositories || 0,
      changeCount: counts.changes || 0,
      changedRepositories: counts.changed || 0,
      conflictedRepositories: counts.conflicted || 0,
      unavailableRepositories: counts.unavailable || 0,
      branch: branch || null,
      branchConsistent: (project.branch || {}).consistent !== false,
      branchOutliers: (project.branch || {}).outliers || [],
      state: (project.state || {}).key || 'unknown',
      stateLabel: (project.state || {}).label || 'unknown',
      dryRun: !!model.dryRun,
    };
  }

  function branchWarning(model) {
    var summary = projectSummary(model);
    if (summary.branchConsistent) { return null; }
    var outliers = summary.branchOutliers;
    var names = outliers.length ? outliers.join(', ') : 'some repositories';
    return 'The repositories are on different branches: ' + names +
      ' did not end up on \'' + (summary.branch || 'the project branch') + '\'. ' +
      'Use “Switch” below to put the whole project on one branch.';
  }

  function notices(model) {
    var list = ((model.project || {}).notices) || [];
    var extra = [];
    var summary = projectSummary(model);
    if (summary.unavailableRepositories > 0) {
      extra.push(countLabel(summary.unavailableRepositories, 'repository', 'repositories') +
        ' could not be inspected (missing directory or not a Git repository).');
    }
    var branch = branchWarning(model);
    if (branch) { extra.push(branch); }
    return list.concat(extra);
  }

  // ------------------------------------------------------------------- tree --

  // Flatten the project tree into renderable lines. The tree is one hierarchy: the
  // external repositories are marked, never promoted to a separate list.
  function treeLines(tree, depth) {
    if (!tree) { return []; }
    depth = depth || 0;
    var lines = [{
      depth: depth,
      name: tree.name,
      path: tree.path,
      repository: tree.repository || null,
      external: !!tree.isExternalRepository,
      files: tree.files || 0,
      truncated: !!tree.truncated
    }];
    var children = tree.children || [];
    for (var i = 0; i < children.length; i++) {
      lines = lines.concat(treeLines(children[i], depth + 1));
    }
    return lines;
  }

  // ------------------------------------------------------------------ changes --

  function describeChange(change) {
    var type = (change.type || {}).key || 'modified';
    var staging = change.conflict ? 'conflict'
      : change.untracked ? 'untracked'
      : (change.staged && change.unstaged) ? 'staged + modified'
      : change.staged ? 'staged'
      : 'not staged';
    return {
      path: change.path,
      relativePath: change.relativePath,
      repository: change.repository,
      repositoryPath: change.repositoryPath,
      type: type,
      typeLabel: (change.type || {}).label || changeLabel(type),
      staging: staging,
      wasPath: change.originalPath || null,
      conflict: !!change.conflict,
      blocksCommit: !!(change.type || {}).blocksCommit
    };
  }

  // Counts follow the staging state of each change (what the repository would record),
  // not the finer change type: a staged rename counts as staged.
  function changeCounts(changes) {
    var counts = { total: changes.length, conflicts: 0, untracked: 0, staged: 0, other: 0 };
    for (var i = 0; i < changes.length; i++) {
      var change = changes[i];
      if (change.conflict) { counts.conflicts++; }
      else if (change.untracked) { counts.untracked++; }
      else if (change.staged) { counts.staged++; }
      else { counts.other++; }
    }
    return counts;
  }

  // Changes stay in one list, but can be grouped by owning repository so it is clear
  // which physical repository a file belongs to — and which message would be used.
  function groupChangesByRepository(changes) {
    var groups = [];
    var index = {};
    for (var i = 0; i < changes.length; i++) {
      var change = describeChange(changes[i]);
      if (!(change.repository in index)) {
        index[change.repository] = groups.length;
        groups.push({
          id: change.repository,
          path: change.repositoryPath,
          changes: [],
          conflicts: 0
        });
      }
      var group = groups[index[change.repository]];
      group.changes.push(change);
      if (change.conflict) { group.conflicts++; }
    }
    return groups;
  }

  // ------------------------------------------------------------------ commit --

  function commitPlan(model) {
    var readiness = (model.readiness || {}).commit || {};
    var targets = readiness.repositories || [];
    var blocked = readiness.blocked || [];
    var files = 0;
    for (var i = 0; i < targets.length; i++) { files += targets[i].files || 0; }
    return {
      enabled: !!readiness.enabled,
      reason: readiness.reason || null,
      repositories: targets,
      blocked: blocked,
      files: files
    };
  }

  function commitMessageProblem(message) {
    if (!message || !message.trim()) {
      return 'GitMesh needs one message for the whole project: every affected repository is ' +
        'committed with it.';
    }
    return null;
  }

  // ---------------------------------------------------------------- progress --

  // Fold the event stream into one row per repository: what the interface shows while
  // an operation runs, and what it shows afterwards.
  function progressRows(events, fallbackRepositories) {
    var order = [];
    var rows = {};
    var state = { operation: null, sentence: null, dryRun: false, finished: false, failed: null };

    function ensure(id, path) {
      if (!(id in rows)) {
        rows[id] = { id: id, path: path || id, status: 'pending', summary: '', details: [], at: 0 };
        order.push(id);
      }
      if (path) { rows[id].path = path; }
      return rows[id];
    }

    (events || []).forEach(function (event) {
      if (event.type === 'started') {
        state.operation = event.operation;
        state.sentence = event.sentence;
        state.dryRun = !!event.dryRun;
        (event.repositories || []).forEach(function (repo) { ensure(repo.id, repo.path); });
      } else if (event.type === 'repository') {
        var row = ensure(event.id, event.path);
        row.status = 'running';
        row.at = event.at || 0;
      } else if (event.type === 'outcome') {
        var done = ensure(event.id, event.path);
        done.status = event.outcome;
        done.summary = event.summary || '';
        done.details = event.details || [];
        done.at = event.at || 0;
      } else if (event.type === 'finished') {
        state.finished = true;
      } else if (event.type === 'failed') {
        state.failed = event.message || 'the operation failed';
      }
    });

    // A finished operation must not leave repositories marked as running.
    if (state.finished) {
      order.forEach(function (id) {
        if (rows[id].status === 'running') { rows[id].status = 'skipped'; }
      });
    }

    (fallbackRepositories || []).forEach(function (repo) { ensure(repo.id, repo.path); });

    return {
      rows: order.map(function (id) { return rows[id]; }),
      operation: state.operation,
      sentence: state.sentence,
      dryRun: state.dryRun,
      finished: state.finished,
      failed: state.failed
    };
  }

  function outcomeSymbol(status) {
    switch (status) {
      case 'success': return '✓';
      case 'skipped': return '–';
      case 'conflict': return '!';
      case 'failed': return '✗';
      case 'running': return '…';
      default: return '·';
    }
  }

  // The sentence shown above the per-repository result, e.g. "Pull completed".
  function resultTitle(operation, rows, dryRun) {
    var problems = rows.filter(function (row) {
      return row.status === 'conflict' || row.status === 'failed';
    }).length;
    var name = (operation || 'operation');
    var title = name.charAt(0).toUpperCase() + name.slice(1);
    if (dryRun) { return title + ' (dry run: nothing was changed)'; }
    if (problems === 0) { return title + ' completed'; }
    if (problems === rows.length) { return title + ' failed'; }
    return title + ' partly completed';
  }

  // Conflicts are never hidden, and the interface never pretends to resolve them.
  function conflictGuidance(rows) {
    var conflicted = rows.filter(function (row) { return row.status === 'conflict'; });
    if (!conflicted.length) { return null; }
    return {
      repositories: conflicted.map(function (row) { return row.id; }),
      message: 'Resolve the conflicting files inside ' +
        (conflicted.length === 1 ? 'the repository' : 'these repositories') +
        ' with Git, then commit and continue. GitMesh never resolves or discards ' +
        'conflicting content automatically.',
      steps: [
        'git status                                    # which files are conflicted',
        'git add <file> && git commit                  # after resolving them',
        'git merge --abort                             # to abandon the merge instead'
      ]
    };
  }

  // ------------------------------------------------------------------- push --

  function pushSummary(model) {
    var rows = (model.repositories || []).map(function (repo) {
      var sync = repo.sync || {};
      return {
        id: repo.id,
        path: repo.path,
        ahead: sync.ahead || 0,
        behind: sync.behind || 0,
        upstream: sync.upstream || null
      };
    });
    var ahead = rows.filter(function (row) { return row.ahead > 0; });
    var behind = rows.filter(function (row) { return row.behind > 0; });
    var withoutUpstream = rows.filter(function (row) { return !row.upstream; });
    return {
      rows: rows,
      pushable: ahead.length,
      behind: behind,
      withoutUpstream: withoutUpstream.map(function (row) { return row.id; })
    };
  }

  // ------------------------------------------------------------------ detail --

  function repositoryDetails(repo) {
    var counts = repo.counts || {};
    var parts = [];
    if (counts.conflict) { parts.push(countLabel(counts.conflict, 'conflict')); }
    if (counts.staged) { parts.push(countLabel(counts.staged, 'staged file')); }
    if (counts.unstaged) { parts.push(countLabel(counts.unstaged, 'modified file')); }
    if (counts.untracked) { parts.push(countLabel(counts.untracked, 'untracked file')); }
    var sync = repo.sync || {};
    if (sync.ahead) { parts.push('ahead ' + sync.ahead); }
    if (sync.behind) { parts.push('behind ' + sync.behind); }
    if (repo.error) { parts.push(repo.error); }
    if (!parts.length) { parts.push('nothing to record'); }
    return parts.join(', ');
  }

  function settingsRows(model) {
    var project = projectSummary(model);
    var rows = [
      { label: 'Project', value: project.name },
      { label: 'Root', value: project.root, mono: true },
      { label: 'Manifest', value: project.manifest, mono: true },
      { label: 'Repositories', value: String(project.repositoryCount) },
      { label: 'Branch', value: project.branch || '—' }
    ];
    (model.repositories || []).forEach(function (repo) {
      rows.push({
        label: repo.id + ' (' + repo.role + ')',
        value: (repo.path === '.' ? 'project root' : repo.path) +
          (repo.remote ? '  →  ' + repo.remote : '  →  no remote'),
        mono: true
      });
    });
    return rows;
  }

  return {
    stateLabel: stateLabel,
    changeLabel: changeLabel,
    countLabel: countLabel,
    projectSummary: projectSummary,
    branchWarning: branchWarning,
    notices: notices,
    treeLines: treeLines,
    describeChange: describeChange,
    changeCounts: changeCounts,
    groupChangesByRepository: groupChangesByRepository,
    commitPlan: commitPlan,
    commitMessageProblem: commitMessageProblem,
    progressRows: progressRows,
    outcomeSymbol: outcomeSymbol,
    resultTitle: resultTitle,
    conflictGuidance: conflictGuidance,
    pushSummary: pushSummary,
    repositoryDetails: repositoryDetails,
    settingsRows: settingsRows
  };
})();

if (typeof module !== 'undefined' && module.exports) {
  module.exports = GitMesh;
}
/* ==== clientLogic:end ==== */

if (typeof document !== 'undefined') {
  (function () {
    'use strict';

    var model = null;
    var activeOperation = null;
    var elapsedTimer = null;
    var operationStartedAt = 0;

    function $(id) { return document.getElementById(id); }

    function api(path, body) {
      var options = { method: body ? 'POST' : 'GET' };
      if (body) {
        options.headers = { 'Content-Type': 'application/x-www-form-urlencoded' };
        options.body = new URLSearchParams(body).toString();
      }
      return fetch(path, options).then(function (response) {
        return response.json().catch(function () { return {}; }).then(function (data) {
          if (!response.ok) {
            var error = new Error(data.error || ('request failed: ' + response.status));
            error.status = response.status;
            error.data = data;
            throw error;
          }
          return data;
        });
      });
    }

    // ------------------------------------------------------------ rendering --

    function escapeHtml(value) {
      return String(value === undefined || value === null ? '' : value)
        .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
        .replace(/"/g, '&quot;');
    }

    function render() {
      if (!model) { return; }
      var opened = !!model.opened;
      $('workspace').hidden = !opened;
      $('welcome').hidden = opened;
      $('dry-run-badge').hidden = !(opened && model.dryRun);

      if (!opened) {
        $('welcome-message').textContent = model.error ||
          'Open the directory of a GitMesh project to see it as one project.';
        $('welcome-path').value = model.directory || '';
        $('welcome-error').hidden = true;
        $('status-line').textContent = 'no project open';
        return;
      }

      var summary = GitMesh.projectSummary(model);
      $('project-name').textContent = summary.name;
      $('project-root').textContent = summary.root;
      $('project-repos').textContent = GitMesh.countLabel
        ? GitMesh.countLabel(summary.repositoryCount, 'repository', 'repositories')
        : summary.repositoryCount + ' repositories';
      $('project-state').innerHTML = statePill(summary.state, summary.stateLabel);
      $('project-branch').textContent = summary.branch || 'no branch yet';
      $('changes-count').textContent = String(summary.changeCount);

      var notices = GitMesh.notices(model);
      $('project-notices').hidden = notices.length === 0;
      $('project-notices').textContent = notices.join('\n');

      renderTree();
      renderStatus();
      renderChanges();
      renderCommit();
      renderBranches();
      renderSync();
      renderSettings();
      $('status-line').textContent = summary.stateLabel + ' · ' +
        summary.changeCount + ' change(s) in ' + summary.changedRepositories +
        ' of ' + summary.repositoryCount + ' repositories';
    }

    function statePill(key, label) {
      return '<span class="badge ' +
        (key === 'clean' ? 'badge-ok' : key === 'changed' ? 'badge-warn' : 'badge-bad') +
        '">' + escapeHtml(label) + '</span>';
    }

    function renderTree() {
      var lines = GitMesh.treeLines(model.tree);
      if (!lines.length) {
        $('tree').innerHTML = '<p class="hint">The project tree is not available.</p>';
        return;
      }
      var html = '';
      lines.forEach(function (line, index) {
        var indent = new Array(line.depth + 1).join('  ');
        var name = index === 0 ? '<span class="tree-root">' + escapeHtml(line.name) + '</span>'
          : escapeHtml(line.name);
        var tag = line.external ? '<span class="tag">repo</span>'
          : (index === 0 ? '<span class="tag">root</span>' : '');
        var files = line.files ? '<span class="files">  ' + line.files + ' file(s)</span>' : '';
        var truncated = line.truncated ? '<span class="files">  …</span>' : '';
        html += '<div class="tree-node">' + indent + name + tag + files + truncated + '</div>';
      });
      $('tree').innerHTML = html;
    }

    function renderStatus() {
      var repositories = model.repositories || [];
      if (!repositories.length) {
        $('status-table').innerHTML = '<p class="hint">No repositories configured.</p>';
        return;
      }
      var html = '<table><thead><tr><th>Repository</th><th>Branch</th><th>State</th>' +
        '<th>Details</th></tr></thead><tbody>';
      repositories.forEach(function (repo) {
        var state = repo.state || {};
        html += '<tr>' +
          '<td>' + escapeHtml(repo.id) +
          (repo.role === 'root' ? ' <span class="owner">(root)</span>' : '') + '</td>' +
          '<td class="mono">' + escapeHtml(repo.head || '—') + '</td>' +
          '<td class="state state-' + escapeHtml(state.key) + '">' + escapeHtml(state.label) + '</td>' +
          '<td class="owner">' + escapeHtml(GitMesh.repositoryDetails(repo)) + '</td>' +
          '</tr>';
      });
      html += '</tbody></table>';
      $('status-table').innerHTML = html;
    }

    function renderChanges() {
      var changes = model.changes || [];
      $('staging-note').textContent = (model.staging || {}).note || '';
      if (!changes.length) {
        $('changes-table').innerHTML = '<p class="hint">There are no changes in this project.</p>';
        return;
      }
      var grouped = $('group-by-repo').checked;
      var html = '<table><thead><tr><th>Path</th><th>Change</th><th>Repository</th>' +
        '<th>Staging</th></tr></thead><tbody>';
      function rowFor(change) {
        var was = change.wasPath ? ' <span class="owner">(was ' + escapeHtml(change.wasPath) + ')</span>' : '';
        var row = '<tr>' +
          '<td class="path">' + escapeHtml(change.path) + was + '</td>' +
          '<td class="state state-' + (change.conflict ? 'conflicted' : 'modified') + '">' +
          escapeHtml(change.typeLabel) + '</td>' +
          '<td class="owner">' + escapeHtml(change.repository) + '</td>' +
          '<td class="owner">' + escapeHtml(change.staging) + '</td></tr>';
        return row;
      }
      if (grouped) {
        GitMesh.groupChangesByRepository(changes).forEach(function (group) {
          html += '<tr><td colspan="4" class="owner"><strong>' + escapeHtml(group.id) + '</strong> ' +
            '<span class="mono">(' + escapeHtml(group.path) + ')</span> — ' +
            group.changes.length + ' file(s)' +
            (group.conflicts ? ', ' + group.conflicts + ' conflict(s)' : '') + '</td></tr>';
          group.changes.forEach(function (change) { html += rowFor(change); });
        });
      } else {
        changes.forEach(function (change) { html += rowFor(GitMesh.describeChange(change)); });
      }
      html += '</tbody></table>';
      $('changes-table').innerHTML = html;
    }

    function renderCommit() {
      var plan = GitMesh.commitPlan(model);
      var html = '';
      if (plan.repositories.length) {
        html += '<p class="hint">One commit will be created in each of these repositories:</p><ul>';
        plan.repositories.forEach(function (repo) {
          html += '<li><strong>' + escapeHtml(repo.id) + '</strong> <span class="owner mono">' +
            escapeHtml(repo.path) + '</span> — ' + repo.files + ' file(s)</li>';
        });
        html += '</ul>';
      }
      if (plan.blocked.length) {
        html += '<p class="resolve"><strong>Cannot be committed yet:</strong></p><ul>';
        plan.blocked.forEach(function (repo) {
          html += '<li><strong>' + escapeHtml(repo.id) + '</strong> <span class="owner mono">' +
            escapeHtml(repo.path) + '</span> — ' +
            (repo.conflicts ? repo.conflicts + ' conflict(s)' : 'unavailable') + '</li>';
        });
        html += '</ul><p class="hint">Resolve these with Git (see the Status tab) and refresh; ' +
          'the other repositories are unaffected.</p>';
      }
      $('commit-targets').innerHTML = html;

      $('btn-commit').disabled = !plan.enabled;
      var hint = $('commit-hint');
      if (plan.reason) {
        hint.textContent = plan.reason;
      } else {
        hint.textContent = plan.files + ' file(s) in ' + plan.repositories.length +
          ' repositor' + (plan.repositories.length === 1 ? 'y' : 'ies');
      }
    }

    function renderBranches() {
      var warning = GitMesh.branchWarning(model);
      var summary = GitMesh.projectSummary(model);
      $('branch-state').textContent = warning ||
        ('Every repository is on \'' + (summary.branch || '—') + '\'.');

      var branches = ((model.branches || {}).list) || [];
      if (!branches.length) {
        $('branches-table').innerHTML = '';
        return;
      }
      var total = summary.repositoryCount;
      var html = '<table><thead><tr><th>Branch</th><th>In repositories</th>' +
        '<th>Checked out in</th></tr></thead><tbody>';
      branches.forEach(function (branch) {
        var missing = total - branch.presentIn.length;
        html += '<tr>' +
          '<td class="mono">' + escapeHtml(branch.name) +
          (branch.everywhere ? '' : ' <span class="owner">(missing in ' + missing + ')</span>') + '</td>' +
          '<td class="owner">' + escapeHtml(branch.presentIn.join(', ')) + '</td>' +
          '<td class="owner">' + escapeHtml(branch.checkedOutIn.join(', ') || '—') + '</td>' +
          '</tr>';
      });
      html += '</tbody></table>';
      $('branches-table').innerHTML = html;
    }

    function renderSync() {
      var summary = GitMesh.pushSummary(model);
      var html = '<table><thead><tr><th>Repository</th><th>Upstream</th><th>Ahead</th>' +
        '<th>Behind</th></tr></thead><tbody>';
      summary.rows.forEach(function (row) {
        html += '<tr><td>' + escapeHtml(row.id) + '</td>' +
          '<td class="owner mono">' + escapeHtml(row.upstream || '—') + '</td>' +
          '<td>' + row.ahead + '</td><td>' + row.behind + '</td></tr>';
      });
      html += '</tbody></table>';
      if (summary.withoutUpstream.length) {
        html += '<p class="hint">No upstream yet in ' +
          escapeHtml(summary.withoutUpstream.join(', ')) +
          ' — GitMesh sets it automatically when it pushes for the first time.</p>';
      }
      if (!summary.pushable) {
        html += '<p class="hint">Nothing to push: no repository has unpublished commits.</p>';
      }
      $('sync-summary').innerHTML = html;
    }

    function renderSettings() {
      var html = '<table><tbody>';
      GitMesh.settingsRows(model).forEach(function (row) {
        html += '<tr><th>' + escapeHtml(row.label) + '</th><td class="' +
          (row.mono ? 'owner mono' : '') + '">' + escapeHtml(row.value) + '</td></tr>';
      });
      html += '</tbody></table>';
      $('settings-table').innerHTML = html;
    }

    // --------------------------------------------------------------- actions --

    function loadModel() {
      return api('/api/model').then(function (data) {
        model = data;
        render();
        return data;
      });
    }

    function refresh() {
      if (activeOperation) { return; }
      return api('/api/refresh', {}).then(function (data) {
        model = data;
        render();
      }).catch(function (error) {
        $('status-line').textContent = 'refresh failed: ' + error.message;
      });
    }

    function showOperation(rows, state) {
      $('operation').hidden = false;
      $('operation-title').textContent = GitMesh.resultTitle(state.operation, rows, state.dryRun);
      var html = '';
      rows.forEach(function (row) {
        var cls = 'progress-' + row.status;
        var symbol = GitMesh.outcomeSymbol(row.status);
        html += '<div class="progress-row ' + cls + '"><span class="symbol">' + symbol +
          '</span><span class="name">' + escapeHtml(row.id) + '</span><span>' +
          escapeHtml(row.summary || labelFor(row.status)) + '</span></div>';
      });
      $('operation-progress').innerHTML = html;
    }

    function labelFor(status) {
      switch (status) {
        case 'running': return 'working…';
        case 'pending': return 'pending';
        case 'skipped': return 'nothing to do';
        default: return '';
      }
    }

    function showResult(state, payload) {
      var box = $('operation-result');
      box.hidden = false;
      var html = '<p class="summary">' + escapeHtml(GitMesh.resultTitle(
        state.operation, state.rows, state.dryRun)) + '</p>';

      if (state.failed) {
        html += '<p class="resolve">' + escapeHtml(state.failed) + '</p>';
      }
      var guidance = GitMesh.conflictGuidance(state.rows);
      if (guidance) {
        html += '<p class="resolve">' + escapeHtml(guidance.message) + '</p>';
        guidance.steps.forEach(function (step) {
          html += '<p class="owner mono">' + escapeHtml(step) + '</p>';
        });
      }
      var details = '';
      state.rows.forEach(function (row) {
        (row.details || []).forEach(function (detail) {
          details += '<p class="owner">' + escapeHtml(row.id) + ': ' + escapeHtml(detail) + '</p>';
        });
      });
      if (details) { html += '<div class="result-details">' + details + '</div>'; }
      box.innerHTML = html;
    }

    function startOperation(path, body, sentence) {
      if (activeOperation) { return; }
      $('operation').hidden = false;
      $('operation-title').textContent = sentence || 'Working…';
      $('operation-progress').innerHTML = '<div class="progress-row progress-running">' +
        '<span class="symbol">…</span><span class="name">starting</span></div>';
      $('operation-result').hidden = true;

      api(path, body || {}).then(function (started) {
        activeOperation = started.id;
        operationStartedAt = Date.now();
        var events = [];
        var state = GitMesh.progressRows(events, (model.repositories || []));
        var source = new EventSource('/api/events/' + started.id);
        if (elapsedTimer) { clearInterval(elapsedTimer); }
        elapsedTimer = setInterval(function () {
          $('operation-elapsed').textContent =
            ((Date.now() - operationStartedAt) / 1000).toFixed(1) + 's';
        }, 200);

        function finish(source, payload) {
          source.close();
          if (elapsedTimer) { clearInterval(elapsedTimer); }
          activeOperation = null;
          var rows = state.rows;
          showOperation(rows, state);
          showResult({ operation: state.operation, rows: rows, dryRun: state.dryRun, failed: state.failed }, payload);
          if (payload && payload.model) {
            model = payload.model;
            render();
            // The operation panel must survive the re-render of the project header.
            showOperation(rows, state);
            $('operation-result').hidden = false;
            showResult({ operation: state.operation, rows: rows, dryRun: state.dryRun, failed: state.failed }, payload);
          } else {
            refresh();
          }
        }

        source.onmessage = function (message) {
          var event;
          try { event = JSON.parse(message.data); } catch (error) { return; }
          events.push(event);
          state = GitMesh.progressRows(events, (model.repositories || []));
          showOperation(state.rows, state);
          if (event.type === 'finished' || event.type === 'failed') {
            finish(source, event);
          }
        };
        source.addEventListener('result', function (message) {
          // A reconnecting interface receives the stored result as one event.
          var payload;
          try { payload = JSON.parse(message.data); } catch (error) { return; }
          if (payload.model) { model = payload.model; render(); }
          finish(source, payload);
        });
        source.addEventListener('closed', function () { source.close(); });
        source.onerror = function () {
          source.close();
          if (activeOperation) {
            activeOperation = null;
            refresh();
          }
        };
      }).catch(function (error) {
        showFailure(error.message);
      });
    }

    function showFailure(message) {
      activeOperation = null;
      $('operation').hidden = false;
      $('operation-title').textContent = 'Operation refused';
      $('operation-progress').innerHTML = '<div class="progress-row progress-failed">' +
        '<span class="symbol">✗</span><span>' + escapeHtml(message) + '</span></div>';
    }

    function commit() {
      var message = $('commit-message').value;
      var problem = GitMesh.commitMessageProblem(message);
      if (problem) { showFailure(problem); return; }
      startOperation('/api/commit', { message: message }, 'Committing the project');
    }

    function branchAction(action) {
      var name = $('branch-name').value.trim();
      if (!name) { showFailure('Enter a branch name first.'); return; }
      startOperation('/api/branch', { action: action, name: name },
        'Branch operation on the project');
    }

    // --------------------------------------------------------------- wiring --

    document.querySelectorAll('.tab').forEach(function (tab) {
      tab.addEventListener('click', function () {
        document.querySelectorAll('.tab').forEach(function (other) {
          other.classList.toggle('active', other === tab);
        });
        ['status', 'changes', 'commit', 'branches', 'sync', 'settings'].forEach(function (name) {
          var panel = $('panel-' + name);
          if (panel) { panel.hidden = name !== tab.dataset.tab; }
        });
      });
    });

    $('btn-refresh').addEventListener('click', refresh);
    $('group-by-repo').addEventListener('change', renderChanges);
    $('btn-commit').addEventListener('click', commit);
    $('btn-fetch').addEventListener('click', function () {
      startOperation('/api/sync', { action: 'fetch' }, 'Fetching the project');
    });
    $('btn-pull').addEventListener('click', function () {
      startOperation('/api/sync', { action: 'pull', strategy: $('pull-strategy').value },
        'Pulling the project');
    });
    $('btn-push').addEventListener('click', function () {
      startOperation('/api/push', {}, 'Pushing the project');
    });
    $('btn-branch-start').addEventListener('click', function () { branchAction('start'); });
    $('btn-branch-create').addEventListener('click', function () { branchAction('create'); });
    $('btn-branch-checkout').addEventListener('click', function () { branchAction('checkout'); });
    $('btn-branch-merge').addEventListener('click', function () { branchAction('merge'); });
    $('btn-branch-delete').addEventListener('click', function () {
      if (window.confirm('Delete this branch in every repository that can delete it safely?')) {
        branchAction('delete');
      }
    });

    var openDialog = $('open-dialog');
    $('btn-open').addEventListener('click', function () {
      $('open-path').value = (model && model.project && model.project.root) || '';
      $('open-error').hidden = true;
      if (typeof openDialog.showModal === 'function') { openDialog.showModal(); }
    });
    $('btn-open-confirm').addEventListener('click', function (event) {
      event.preventDefault();
      openProject($('open-path').value, $('open-error'));
    });
    $('btn-welcome-open').addEventListener('click', function () {
      openProject($('welcome-path').value, $('welcome-error'));
    });
    $('btn-welcome-here').addEventListener('click', function () {
      openProject($('welcome-path').value, $('welcome-error'));
    });

    function openProject(path, errorNode) {
      api('/api/open', { path: path }).then(function () {
        if (typeof openDialog.close === 'function') { openDialog.close(); }
        $('operation').hidden = true;
        return loadModel();
      }).catch(function (error) {
        errorNode.textContent = error.message;
        errorNode.hidden = false;
        loadModel();
      });
    }

    // dry-run toggle lives in the status bar: double-click the badge to switch.
    $('dry-run-badge').addEventListener('dblclick', function () {
      api('/api/dry-run', { value: String(!(model && model.dryRun)) }).then(function (data) {
        model = data;
        render();
      });
    });

    document.addEventListener('keydown', function (event) {
      if (event.target.tagName === 'INPUT' || event.target.tagName === 'TEXTAREA') {
        if (event.key === 'Enter' && (event.ctrlKey || event.metaKey)) {
          commit();
        }
        return;
      }
      if (event.key === 'r' || event.key === 'R') { refresh(); }
      if (event.key === 'd' || event.key === 'D') {
        api('/api/dry-run', { value: String(!(model && model.dryRun)) }).then(function (data) {
          model = data; render();
        });
      }
      var index = ['1', '2', '3', '4', '5', '6'].indexOf(event.key);
      if (index >= 0) {
        var tabs = document.querySelectorAll('.tab');
        if (tabs[index]) { tabs[index].click(); }
      }
    });

    // A gentle refresh keeps the status honest without polling aggressively.
    setInterval(function () {
      if (!activeOperation && !document.hidden && model && model.opened) { refresh(); }
    }, 15000);

    loadModel();
  })();
}
