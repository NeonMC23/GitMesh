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

    function ensure(id, path, role, detail) {
      if (!(id in rows)) {
        rows[id] = {
          id: id, path: path || id, status: 'pending', summary: '', details: [], at: 0,
          role: role || '', detail: detail || ''
        };
        order.push(id);
      }
      if (path) { rows[id].path = path; }
      if (role) { rows[id].role = role; }
      if (detail) { rows[id].detail = detail; }
      return rows[id];
    }

    (events || []).forEach(function (event) {
      if (event.type === 'started') {
        state.operation = event.operation;
        state.sentence = event.sentence;
        state.dryRun = !!event.dryRun;
        (event.repositories || []).forEach(function (repo) {
          ensure(repo.id, repo.path, repo.role, repo.detail);
        });
      } else if (event.type === 'repository') {
        var row = ensure(event.id, event.path, event.role, event.detail);
        row.status = 'running';
        row.at = event.at || 0;
      } else if (event.type === 'outcome') {
        var done = ensure(event.id, event.path, event.role);
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

  // -------------------------------------------------------------------- setup --

  // Facts about a scanned directory, in the words the wizard needs. Everything here comes
  // from the inspection the Rust side produced: the interface discovers nothing itself.
  function inspectionSummary(inspection) {
    var payload = inspection || {};
    var candidates = payload.candidates || [];
    var nested = 0;
    (function count(nodes) {
      (nodes || []).forEach(function (node) {
        nested += (node.nestedRepositories || []).length;
        count(node.children);
      });
    })(candidates);
    return {
      root: payload.root || '',
      exists: payload.exists !== false,
      isProject: !!payload.isGitMeshProject,
      projectName: (payload.manifest || {}).projectName || null,
      suggestedName: payload.suggestedName || '',
      files: payload.files || 0,
      isRepository: !!payload.rootIsRepository,
      hasCommits: !!payload.rootHasCommits,
      manifestExists: !!((payload.manifest || {}).exists),
      manifestError: ((payload.manifest || {}).error) || null,
      enclosing: payload.enclosingRepository || null,
      repositories: payload.repositories || [],
      candidates: candidates,
      candidateCount: candidates.length,
      nestedCount: nested,
      notices: payload.notices || [],
      truncated: !!payload.truncated
    };
  }

  // The selectable directories, flattened from the inspection tree: the wizard shows one
  // list, and the project stays one hierarchy.
  function selectableDirectories(inspection) {
    var lines = [];
    (function walk(nodes, depth) {
      (nodes || []).forEach(function (node) {
        lines.push({
          path: node.path,
          name: node.name,
          depth: depth,
          files: node.files || 0,
          subtreeFiles: node.subtreeFiles || 0,
          isRepository: !!node.isRepository,
          hasGitDir: !!node.hasGitDir,
          hasCommits: !!node.hasCommits,
          branch: node.branch || null,
          remote: node.remote || null,
          trackedByRoot: node.trackedByRoot || 0,
          nestedRepositories: node.nestedRepositories || [],
          suggested: !!node.suggested,
          suggestedId: node.suggestedId || '',
          children: (node.children || []).length
        });
        walk(node.children, depth + 1);
      });
    })((inspection || {}).candidates);
    return lines;
  }

  // What the answers mean for the request: the interface checks its own form before
  // sending it, and the server checks everything again.
  function setupFormProblem(form) {
    form = form || {};
    var root = (form.root || '').trim();
    if (!root) { return 'Enter the directory of the project.'; }
    if (root.charAt(0) !== '/') { return 'The project root must be an absolute path.'; }
    var seen = {};
    var repositories = form.repositories || [];
    for (var i = 0; i < repositories.length; i++) {
      var repo = repositories[i];
      if (!repo.path) { return 'A selected directory has no path.'; }
      var id = (repo.id || '').trim();
      if (!id) { return 'Give "' + repo.path + '" a name: every repository in the manifest has one.'; }
      if (seen[id]) { return 'The name "' + id + '" is used twice: repository names must be unique.'; }
      seen[id] = true;
      if (repo.provider === 'github') {
        if (!(repo.owner || '').trim() || !(repo.name || '').trim()) {
          return 'For "' + id + '", enter the GitHub owner and repository name.';
        }
      }
    }
    if (form.publish) {
      if (!(form.firstCommit || '').trim()) {
        return 'Enter the message of the first commit, or turn the first publish off.';
      }
    }
    return null;
  }

  // One record per repository, the way the server expects it: values escaped, separators
  // literal. The body is assembled by hand for exactly this reason.
  function repositoryRecord(repo) {
    var fields = [];
    function add(key, value) {
      fields.push(key + '=' + encodeURIComponent(value === undefined || value === null ? '' : String(value)));
    }
    add('path', repo.path);
    add('id', repo.id);
    add('create', repo.create ? 'yes' : 'no');
    add('untrack', repo.untrack ? 'yes' : 'no');
    if (repo.provider === 'github') {
      add('provider', 'github');
      add('owner', repo.owner);
      add('name', repo.name);
      add('scheme', repo.scheme || 'ssh');
      add('visibility', repo.visibility || 'private');
    } else if (repo.remote) {
      add('remote', repo.remote);
    }
    return fields.join(';');
  }

  // Remote kinds the wizard offers, and the safest default visibility.
  function remoteKindLabel(kind) {
    switch (kind) {
      case 'none': return 'no remote';
      case 'manual': return 'Git URL';
      case 'github': return 'GitHub';
      default: return kind || 'no remote';
    }
  }

  // The review, reduced to what a human decides on. The plan is the only source: the
  // interface never re-derives what GitMesh will do.
  function setupPlanSummary(plan) {
    var payload = plan || {};
    var repositories = payload.repositories || [];
    var counts = payload.counts || {};
    var publish = payload.publish || null;
    return {
      id: payload.id || '',
      ready: !!payload.ready,
      noop: !!payload.noop,
      summary: payload.summary || '',
      name: (payload.project || {}).name || '',
      root: (payload.project || {}).root || '',
      manifestPath: (payload.project || {}).manifest || '',
      manifest: payload.manifest || '',
      repositories: repositories,
      creates: repositories.filter(function (repo) { return repo.create; }),
      adopts: repositories.filter(function (repo) { return !repo.create && repo.exists; }),
      replacesRemote: repositories.filter(function (repo) { return repo.remoteAction === 'update'; }),
      addsRemote: repositories.filter(function (repo) { return repo.remoteAction === 'add'; }),
      keepsRemote: repositories.filter(function (repo) { return repo.remoteAction === 'keep'; }),
      untracks: repositories.filter(function (repo) { return repo.untrack; }),
      hosted: repositories.filter(function (repo) { return !!repo.hosted; }),
      steps: payload.steps || [],
      planned: (counts.planned || 0),
      already: (counts.already || 0),
      blocked: (counts.blocked || 0),
      createdRepositories: (counts.createRepositories || 0),
      safety: payload.safety || [],
      blockers: payload.blockers || [],
      warnings: payload.warnings || [],
      notices: payload.notices || [],
      publish: publish ? {
        message: publish.message || '',
        repositories: publish.repositories || [],
        sentence: publish.sentence || '',
        safety: publish.safety || ''
      } : null
    };
  }

  function stepStateLabel(state) {
    switch (state) {
      case 'planned': return 'will run';
      case 'already-satisfied':
      case 'already': return 'already in place';
      case 'blocked': return 'refused';
      default: return state || 'unknown';
    }
  }

  // The commands that create a hosted repository, straight from the provider layer: the
  // wizard never invents a URL or a command of its own.
  function hostedCommands(plan) {
    var repositories = ((plan || {}).repositories) || [];
    var commands = [];
    repositories.forEach(function (repo) {
      var hosted = repo.hosted;
      if (!hosted) { return; }
      commands.push({
        id: repo.id,
        fullName: hosted.fullName || '',
        visibility: hosted.visibility || 'private',
        url: hosted.url || '',
        webUrl: hosted.webUrl || '',
        command: hosted.command || '',
        note: hosted.note || '',
        createsRepository: !!hosted.createsRepository
      });
    });
    return commands;
  }

  // The result of an apply, in the words the result panel uses. Rows come from the event
  // stream; the counts, the summary and the validation come from the core.
  function setupResultText(payload, rows) {
    var setup = (payload || {}).setup || {};
    var counts = setup.counts || {};
    var validation = setup.validation || null;
    var problems = (rows || []).filter(function (row) {
      return row.status === 'conflict' || row.status === 'failed';
    });
    return {
      status: setup.status || 'unknown',
      sentence: setup.sentence || 'Project setup finished',
      summary: setup.summary || '',
      exitCode: setup.exitCode === undefined ? null : setup.exitCode,
      succeeded: counts.succeeded || 0,
      skipped: counts.skipped || 0,
      failed: counts.failed || 0,
      problems: problems.map(function (row) { return row.id; }),
      validation: validation,
      validationOk: !!(validation && validation.ok),
      validationIssues: (validation && validation.issues) || [],
      manifest: setup.manifest || '',
      opened: !!(payload || {}).opened,
      publish: (payload || {}).publish || null
    };
  }

  // What the result panel says about the first publish, when the plan had one.
  function publishResultText(publish) {
    if (!publish) { return null; }
    var sections = publish || [];
    var result = { commits: 0, pushes: 0, problems: [], succeeded: 0, skipped: 0 };
    sections.forEach(function (section) {
      var outcomes = section.outcomes || [];
      var counts = section.counts || {};
      if (section.operation === 'First commit') { result.commits = outcomes.length; }
      if (section.operation === 'First push') { result.pushes = outcomes.length; }
      result.succeeded += counts.succeeded || 0;
      result.skipped += counts.skipped || 0;
      if (section.error) { result.problems.push(section.operation + ': ' + section.error); }
      outcomes.forEach(function (outcome) {
        if (outcome.outcome === 'failed' || outcome.outcome === 'conflict') {
          result.problems.push(section.operation + ' — ' + outcome.id + ': ' +
            (outcome.summary || outcome.outcome));
        }
      });
    });
    return result;
  }

  function setupProgressRows(events, fallback) {
    var folded = progressRows(events, fallback);
    var meta = {};
    (events || []).forEach(function (event) {
      if (event.type === 'started') {
        (event.repositories || []).forEach(function (repo) { meta[repo.id] = repo; });
      } else if ((event.type === 'repository' || event.type === 'outcome') && meta[event.id] === undefined) {
        meta[event.id] = { id: event.id, role: event.role || '', detail: event.detail || '' };
      }
    });
    folded.rows.forEach(function (row) {
      var extra = meta[row.id] || {};
      row.role = row.role || extra.role || '';
      row.detail = row.detail || extra.detail || '';
      row.label = row.role ? (row.role + ' · ' + row.id) : row.id;
    });
    return folded;
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
    settingsRows: settingsRows,
    inspectionSummary: inspectionSummary,
    selectableDirectories: selectableDirectories,
    setupFormProblem: setupFormProblem,
    repositoryRecord: repositoryRecord,
    remoteKindLabel: remoteKindLabel,
    setupPlanSummary: setupPlanSummary,
    stepStateLabel: stepStateLabel,
    hostedCommands: hostedCommands,
    setupResultText: setupResultText,
    publishResultText: publishResultText,
    setupProgressRows: setupProgressRows
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
        // A string body is already encoded by the caller: the wizard builds it field by
        // field so that the separators inside a repository record stay literal.
        options.body = typeof body === 'string' ? body : new URLSearchParams(body).toString();
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
      $('workspace').hidden = !opened || wizardActive();
      $('welcome').hidden = opened || wizardActive();
      $('setup').hidden = !wizardActive();
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

    // ------------------------------------------------------------- setup wizard --

    // The wizard keeps the user's answers and the plan the Rust side produced. It never
    // computes what GitMesh will do: the plan is the only source for the review screen.
    var wizard = {
      active: false,
      inspection: null,
      plan: null,
      selection: {},          // path -> the user's yes/no (kept across rescans)
      forms: {},              // path -> the answers for one directory
      rootRemote: {},         // the root repository's remote answers
      confirmRemotes: false,
      overwriteManifest: false,
      publish: false,
      firstCommit: ''
    };

    function wizardActive() { return !!wizard.active; }

    function openWizard() {
      wizard.active = true;
      wizard.plan = null;
      $('setup').hidden = false;
      $('workspace').hidden = true;
      $('welcome').hidden = true;
      $('setup-step-11').hidden = true;
      $('setup-step-12').hidden = true;
      $('setup-plan').innerHTML = '';
      $('setup-plan-id').textContent = '—';
      $('setup-confirm').checked = false;
      $('btn-setup-apply').disabled = true;
      if (!$('setup-root').value) { $('setup-root').value = (model && model.directory) || ''; }
      $('setup-root').focus();
      if ($('setup-root').value.trim()) { scanDirectory(); }
    }

    function closeWizard() {
      wizard.active = false;
      $('setup').hidden = true;
      render();
    }

    // ------------------------------------------------------------- inspection --

    function scanDirectory() {
      var path = $('setup-root').value.trim();
      if (!path) { return; }
      $('setup-scan-error').hidden = true;
      $('setup-scan-hint').textContent = 'scanning…';
      var name = $('setup-name').value.trim();
      api('/api/setup/inspect', { path: path, name: name }).then(function (data) {
        wizard.inspection = data.inspection;
        wizard.plan = null;
        var summary = GitMesh.inspectionSummary(wizard.inspection);
        $('setup-scan-hint').textContent = 'scanned ' + summary.root;
        if (!name) { $('setup-name').value = summary.suggestedName; }
        $('setup-root-create').checked = !summary.isRepository;
        if (!$('setup-root-branch').value) { $('setup-root-branch').value = 'main'; }
        rebuildForms();
        renderWizard();
      }).catch(function (error) {
        $('setup-scan-hint').textContent = '';
        $('setup-scan-error').textContent = error.message;
        $('setup-scan-error').hidden = false;
      });
    }

    // Defaults for one directory. What the user has already edited is kept; everything else
    // comes from the inspection, and the ids come from the Rust side (never invented here).
    function defaultForm(directory, previous) {
      var form = {
        id: directory.suggestedId,
        idEdited: false,
        create: !directory.hasGitDir,
        remoteKind: directory.remote ? 'existing' : 'none',
        remote: directory.remote || '',
        remoteEdited: false,
        owner: '',
        name: lastSegment(directory.path),
        scheme: 'ssh',
        visibility: 'private',
        untrack: directory.trackedByRoot > 0
      };
      if (!previous) { return form; }
      if (previous.idEdited) { form.id = previous.id; form.idEdited = true; }
      if (previous.remoteEdited) {
        form.remoteKind = previous.remoteKind;
        form.remote = previous.remote;
        form.owner = previous.owner;
        form.name = previous.name;
        form.scheme = previous.scheme;
        form.visibility = previous.visibility;
        form.remoteEdited = true;
      }
      form.create = previous.create;
      form.untrack = previous.untrack;
      return form;
    }

    function rebuildForms() {
      var directories = GitMesh.selectableDirectories(wizard.inspection);
      var previousForms = wizard.forms;
      var forms = {};
      directories.forEach(function (directory) {
        forms[directory.path] = defaultForm(directory, previousForms[directory.path]);
        if (!(directory.path in wizard.selection)) {
          wizard.selection[directory.path] = !!directory.suggested;
        }
      });
      var selection = {};
      Object.keys(wizard.selection).forEach(function (path) {
        if (forms[path]) { selection[path] = wizard.selection[path]; }
      });
      wizard.forms = forms;
      wizard.selection = selection;
    }

    function selectedPaths() {
      return Object.keys(wizard.selection).filter(function (path) {
        return wizard.selection[path] && wizard.forms[path];
      }).sort();
    }

    function lastSegment(path) {
      var parts = String(path).split('/');
      return parts[parts.length - 1] || path;
    }

    function renderWizard() {
      renderInspection();
      renderStructure();
      renderRootRepository();
      renderRepositories();
      renderExisting();
      renderRemotes();
      renderHosting();
      renderManifestStep();
      renderReviewState();
    }

    // ---------------------------------------------------------------- step 1 --

    function renderInspection() {
      var node = $('setup-inspection');
      var inspection = wizard.inspection;
      if (!inspection) { node.innerHTML = ''; return; }
      var summary = GitMesh.inspectionSummary(inspection);
      var html = '<ul class="facts">';
      html += '<li>Path: <strong class="mono">' + escapeHtml(summary.root) + '</strong> ' +
        (summary.exists ? '(exists)' : '(does not exist — the path is refused)') + '</li>';
      html += '<li>Git repository in this directory: <strong>' +
        (summary.isRepository ? 'yes' : 'no') + '</strong></li>';
      html += '<li>GitMesh manifest: <strong>' + (summary.manifestExists ? 'present, untouched' : 'none') +
        '</strong>' + (summary.manifestError ? ' — ' + escapeHtml(summary.manifestError) : '') + '</li>';
      if (summary.enclosing) {
        html += '<li>This directory sits inside the Git repository at <strong class="mono">' +
          escapeHtml(summary.enclosing) + '</strong>, which GitMesh will not touch</li>';
      }
      html += '<li>Directories that could become repositories: <strong>' +
        summary.candidateCount + '</strong></li>';
      if (summary.repositories.length) {
        html += '<li>Git repositories found in the tree: <strong>' +
          escapeHtml(summary.repositories.map(function (repo) { return repo.id; }).join(', ')) +
          '</strong></li>';
      }
      if (summary.nestedCount) {
        html += '<li>Nested Git repositories: <strong>' + summary.nestedCount +
          '</strong> — reported, never touched</li>';
      }
      html += '</ul>';
      if (summary.isProject) {
        html += '<p class="hint">This directory already is a GitMesh project' +
          (summary.projectName ? ' (' + escapeHtml(summary.projectName) + ')' : '') +
          '. Open it instead of setting it up again.</p>' +
          '<div class="row"><button id="btn-setup-open-existing" class="primary">' +
          'Open the existing project</button></div>';
      }
      summary.notices.forEach(function (notice) {
        html += '<p class="hint">' + escapeHtml(notice) + '</p>';
      });
      node.innerHTML = html;
      var openExisting = $('btn-setup-open-existing');
      if (openExisting) {
        openExisting.addEventListener('click', function () {
          api('/api/open', { path: summary.root }).then(function () {
            return loadModel();
          }).then(function () {
            if (model && model.opened) { closeWizard(); }
          }).catch(function (error) {
            $('setup-scan-error').textContent = error.message;
            $('setup-scan-error').hidden = false;
          });
        });
      }
    }

    // ---------------------------------------------------------------- step 2 --

    function renderStructure() {
      var directories = GitMesh.selectableDirectories(wizard.inspection);
      var node = $('setup-structure');
      if (!directories.length) {
        node.innerHTML = '<p class="hint">No sub-directories to choose from: the project will be ' +
          'the root repository alone, which is a perfectly good GitMesh project.</p>';
        $('setup-coverage').textContent = '';
        return;
      }
      var html = '';
      directories.forEach(function (directory) {
        var tags = [];
        if (directory.isRepository) { tags.push('<span class="tag">git repository</span>'); }
        if (directory.trackedByRoot) {
          tags.push('<span class="tag">' + directory.trackedByRoot + ' tracked by root</span>');
        }
        if (directory.nestedRepositories.length) {
          tags.push('<span class="tag">contains a git repository</span>');
        }
        if (directory.suggested) { tags.push('<span class="tag">suggested</span>'); }
        html += '<div class="selection-row">' +
          '<input type="checkbox" data-path="' + escapeHtml(directory.path) + '"' +
          (wizard.selection[directory.path] ? ' checked' : '') + '>' +
          '<span class="mono">' + escapeHtml(directory.path) + '</span>' +
          '<span class="tags">' + tags.join('') +
          (directory.subtreeFiles ? directory.subtreeFiles + ' file(s)' : 'empty') + '</span></div>';
      });
      node.innerHTML = html;
      node.querySelectorAll('input[type=checkbox]').forEach(function (box) {
        box.addEventListener('change', function () {
          wizard.selection[box.dataset.path] = box.checked;
          renderWizard();
          invalidatePlan();
        });
      });
      var selected = selectedPaths();
      $('setup-coverage').textContent = selected.length
        ? selected.length + ' director' + (selected.length === 1 ? 'y' : 'ies') +
          ' become repositories; everything else stays in the root repository.'
        : 'Nothing selected: the whole project would live in the root repository.';
    }

    // ------------------------------------------------------------ remote rows --

    // The remote fields, rendered from the answers: the wizard collects intent, and the
    // provider layer turns it into the URL the plan shows.
    function remoteFieldsHtml(prefix, form) {
      var fields = '<div class="grid-fields">';
      fields += '<label class="field"><span>Remote</span><select data-remote-kind="' + escapeHtml(prefix) + '">' +
        ['none', 'manual', 'github'].map(function (kind) {
          return '<option value="' + kind + '"' + (form.remoteKind === kind ? ' selected' : '') +
            '>' + escapeHtml(GitMesh.remoteKindLabel(kind)) + '</option>';
        }).join('') +
        (form.remoteKind === 'existing'
          ? '<option value="existing" selected>the remote it already has</option>' : '') +
        '</select></label>';
      if (form.remoteKind === 'manual') {
        fields += '<label class="field"><span>Git URL</span><input type="text" data-remote-url="' +
          escapeHtml(prefix) + '" value="' + escapeHtml(form.remote) +
          '" placeholder="git@host:group/repo.git"></label>';
      }
      if (form.remoteKind === 'existing') {
        fields += '<label class="field"><span>Remote found on disk (kept)</span><input type="text" value="' +
          escapeHtml(form.remote || 'none') + '" disabled></label>';
      }
      if (form.remoteKind === 'github') {
        fields += '<label class="field"><span>GitHub owner</span><input type="text" data-remote-owner="' +
          escapeHtml(prefix) + '" value="' + escapeHtml(form.owner) + '" placeholder="acme"></label>' +
          '<label class="field"><span>GitHub repository</span><input type="text" data-remote-name="' +
          escapeHtml(prefix) + '" value="' + escapeHtml(form.name) + '"></label>' +
          '<label class="field"><span>URL to configure</span><select data-remote-scheme="' +
          escapeHtml(prefix) + '">' +
          ['ssh', 'https'].map(function (scheme) {
            return '<option value="' + scheme + '"' + (form.scheme === scheme ? ' selected' : '') +
              '>' + scheme + '</option>';
          }).join('') + '</select></label>' +
          '<label class="field"><span>Visibility</span><select data-remote-visibility="' +
          escapeHtml(prefix) + '">' +
          ['private', 'internal', 'public'].map(function (visibility) {
            return '<option value="' + visibility + '"' +
              (form.visibility === visibility ? ' selected' : '') +
              '>' + visibility + '</option>';
          }).join('') + '</select></label>';
      }
      return fields + '</div>';
    }

    /// The form a remote field belongs to: the root repository's, or one directory's.
    function remoteForm(prefix) {
      if (prefix === '.') {
        wizard.rootRemote = wizard.rootRemote || {};
        return wizard.rootRemote;
      }
      return wizard.forms[prefix] || null;
    }

    function wireRemoteFields(root) {
      function update(selector, key) {
        root.querySelectorAll(selector).forEach(function (field) {
          field.addEventListener('change', function () {
            var form = remoteForm(field.dataset.remoteKind || field.dataset.remoteUrl ||
              field.dataset.remoteOwner || field.dataset.remoteName || field.dataset.remoteScheme ||
              field.dataset.remoteVisibility);
            if (!form) { return; }
            form[key] = field.value;
            form.remoteEdited = true;
            if (key === 'remoteKind') {
              // Switching away from "the remote it already has" clears the old URL.
              if (field.value !== 'existing' && field.value !== 'manual') { form.remote = ''; }
              if (field.value === 'existing' && wizard.forms[field.dataset.remoteKind] &&
                  wizard.forms[field.dataset.remoteKind].remote) {
                form.remote = wizard.forms[field.dataset.remoteKind].remote;
              }
            }
            renderWizard();
            invalidatePlan();
          });
        });
      }
      update('[data-remote-kind]', 'remoteKind');
      update('[data-remote-url]', 'remote');
      update('[data-remote-owner]', 'owner');
      update('[data-remote-name]', 'name');
      update('[data-remote-scheme]', 'scheme');
      update('[data-remote-visibility]', 'visibility');
    }

    // ---------------------------------------------------------------- step 3 --

    function rootRemoteForm() {
      var inspection = wizard.inspection;
      var summary = inspection ? GitMesh.inspectionSummary(inspection) : { repositories: [], root: '' };
      var root = (summary.repositories || []).filter(function (repo) { return repo.isRoot; })[0] || {};
      var form = wizard.rootRemote || {};
      return {
        remoteKind: form.remoteKind || (root.remote ? 'existing' : 'none'),
        remote: form.remote === undefined ? (root.remote || '') : form.remote,
        owner: form.owner || '',
        name: form.name || $('setup-name').value.trim() || lastSegment(summary.root),
        scheme: form.scheme || 'ssh',
        visibility: form.visibility || 'private'
      };
    }

    function renderRootRepository() {
      var node = $('setup-root-state');
      var inspection = wizard.inspection;
      if (!inspection) { node.innerHTML = ''; return; }
      var summary = GitMesh.inspectionSummary(inspection);
      var root = (summary.repositories || []).filter(function (repo) { return repo.isRoot; })[0] || null;
      var html = '<ul class="facts">';
      html += '<li>' + (summary.isRepository
        ? 'The root directory already is a Git repository: it is used as it is and never re-initialised.'
        : 'The root directory is not a Git repository yet — GitMesh can create one for you.') + '</li>';
      if (root) {
        html += '<li>Branch: <strong>' + escapeHtml(root.branch || 'none yet') + '</strong> · remote: <strong>' +
          escapeHtml(root.remote || 'none') + '</strong></li>';
      }
      html += '</ul>';
      node.innerHTML = html;

      var fields = $('setup-root-remote-fields');
      fields.innerHTML = remoteFieldsHtml('.', rootRemoteForm());
      wireRemoteFields(fields);
    }

    // ---------------------------------------------------------------- step 4 --

    function renderRepositories() {
      var node = $('setup-repositories');
      var directories = GitMesh.selectableDirectories(wizard.inspection);
      var html = '';
      selectedPaths().forEach(function (path) {
        var directory = directories.filter(function (item) { return item.path === path; })[0];
        var form = wizard.forms[path];
        if (!directory) { return; }
        html += '<div class="repo-editor">' +
          '<div class="repo-title"><strong class="mono">' + escapeHtml(path) + '</strong>' +
          '<span class="owner">' + (directory.hasGitDir ? 'a git repository already' : 'no .git yet') +
          (directory.hasCommits ? ' · has commits' : '') +
          (directory.branch ? ' · branch ' + escapeHtml(directory.branch) : '') +
          (directory.trackedByRoot ? ' · ' + directory.trackedByRoot + ' file(s) tracked by root' : '') +
          '</span></div>' +
          '<div class="grid-fields">' +
          '<label class="field"><span>Name in the project</span><input type="text" data-id="' +
          escapeHtml(path) + '" value="' + escapeHtml(form.id) + '"></label>' +
          '<label class="check"><input type="checkbox" data-create="' + escapeHtml(path) + '"' +
          (form.create ? ' checked' : '') + '> create a Git repository here if there is none</label>' +
          '<label class="check"><input type="checkbox" data-untrack="' + escapeHtml(path) + '"' +
          (form.untrack ? ' checked' : '') +
          '> stop tracking it in the root repository (index only — files stay on disk)</label>' +
          '</div>' +
          remoteFieldsHtml(path, form) +
          '</div>';
      });
      node.innerHTML = html || '<p class="hint">No repository selected: the project would be the ' +
        'root repository alone.</p>';

      node.querySelectorAll('[data-id]').forEach(function (field) {
        field.addEventListener('change', function () {
          var form = wizard.forms[field.dataset.id];
          if (!form) { return; }
          form.id = field.value.trim();
          form.idEdited = true;
          invalidatePlan();
        });
      });
      node.querySelectorAll('[data-create]').forEach(function (field) {
        field.addEventListener('change', function () {
          var form = wizard.forms[field.dataset.create];
          if (form) { form.create = field.checked; }
          invalidatePlan();
        });
      });
      node.querySelectorAll('[data-untrack]').forEach(function (field) {
        field.addEventListener('change', function () {
          var form = wizard.forms[field.dataset.untrack];
          if (form) { form.untrack = field.checked; }
          invalidatePlan();
        });
      });
      wireRemoteFields(node);
    }

    // ---------------------------------------------------------------- step 5 --

    function renderExisting() {
      var node = $('setup-existing');
      var directories = GitMesh.selectableDirectories(wizard.inspection);
      var existing = selectedPaths().map(function (path) {
        return directories.filter(function (item) { return item.path === path; })[0];
      }).filter(function (directory) { return directory && directory.isRepository; });
      var html = '';
      if (!existing.length) {
        html += '<p class="hint">None of the selected directories is a Git repository yet, so ' +
          'GitMesh will create them.</p>';
      } else {
        html += '<ul class="facts">';
        existing.forEach(function (directory) {
          html += '<li><strong class="mono">' + escapeHtml(directory.path) +
            '</strong> is kept exactly as it is: its history, its branch (' +
            escapeHtml(directory.branch || 'none yet') + ') and its ' +
            (directory.remote ? '<span class="mono">' + escapeHtml(directory.remote) + '</span>' : 'lack of a remote') +
            ' are untouched';
          if (directory.nestedRepositories.length) {
            html += '. It contains another Git repository at <span class="mono">' +
              escapeHtml(directory.nestedRepositories.join(', ')) +
              '</span>, which GitMesh reports and never manages';
          }
          html += '.</li>';
        });
        html += '</ul>';
        html += '<label class="check"><input type="checkbox" id="setup-confirm-remotes"' +
          (wizard.confirmRemotes ? ' checked' : '') +
          '> I confirm replacing the <code>origin</code> of a repository whose remote above differs' +
          '</label>';
      }
      node.innerHTML = html;
      if ($('setup-confirm-remotes')) {
        $('setup-confirm-remotes').addEventListener('change', function (event) {
          wizard.confirmRemotes = event.target.checked;
          invalidatePlan();
        });
      }
    }

    // ---------------------------------------------------------------- step 6 --

    function remoteDescription(form) {
      if (form.remoteKind === 'github') {
        return 'GitHub ' + (form.owner || '?') + '/' + (form.name || '?') + ' over ' + form.scheme +
          ' (' + form.visibility + ')';
      }
      if (form.remoteKind === 'manual') { return form.remote || '(no URL yet)'; }
      if (form.remoteKind === 'existing') { return form.remote || 'the remote it already has'; }
      return 'no remote (local only)';
    }

    function renderRemotes() {
      var node = $('setup-remotes');
      var rows = [];
      var root = rootRemoteForm();
      if (root.remoteKind !== 'none') {
        rows.push({ id: '(root)', path: '.', remote: remoteDescription(root) });
      }
      selectedPaths().forEach(function (path) {
        rows.push({ id: wizard.forms[path].id, path: path, remote: remoteDescription(wizard.forms[path]) });
      });
      if (!rows.length) {
        node.innerHTML = '<p class="hint">No remote anywhere: the project works entirely locally.</p>';
        return;
      }
      var html = '<table><thead><tr><th>Repository</th><th>Path</th><th>Remote to record</th>' +
        '</tr></thead><tbody>';
      rows.forEach(function (row) {
        html += '<tr><td>' + escapeHtml(row.id) + '</td><td class="owner mono">' + escapeHtml(row.path) +
          '</td><td class="owner mono">' + escapeHtml(row.remote) + '</td></tr>';
      });
      node.innerHTML = html + '</tbody></table>';
    }

    // ---------------------------------------------------------------- step 7 --

    function renderHosting() {
      var node = $('setup-hosting');
      var hosted = [];
      if (rootRemoteForm().remoteKind === 'github') { hosted.push('(root)'); }
      selectedPaths().forEach(function (path) {
        if (wizard.forms[path].remoteKind === 'github') { hosted.push(wizard.forms[path].id); }
      });
      var html = '<ul class="facts">' +
        '<li>GitMesh never creates a repository on GitHub, never asks for a token and stores no ' +
        'credential anywhere.</li>' +
        '<li>It prepares the URL, records it in the manifest and shows you the exact command that ' +
        'creates the repository on GitHub, which you run if and when you want one.</li>' +
        '<li>A project with no hosted remote is complete and fully usable.</li>' +
        '</ul>';
      html += hosted.length
        ? '<p class="hint">Hosted on GitHub in this setup: <strong>' + escapeHtml(hosted.join(', ')) +
          '</strong>. The review step shows the exact URL and command, from the provider layer.</p>'
        : '<p class="hint">No GitHub remote here: nothing to create, nothing to authenticate.</p>';
      node.innerHTML = html;
    }

    // ---------------------------------------------------------------- step 8 --

    function renderManifestStep() {
      var node = $('setup-manifest');
      var inspection = wizard.inspection;
      var html = '';
      var manifestPath = (inspection && inspection.manifest.path) || '.gitmesh/project.toml';
      if (inspection && inspection.manifest.exists) {
        html += '<p class="hint">A manifest already exists at <span class="mono">' +
          escapeHtml(manifestPath) + '</span>. GitMesh keeps it untouched unless you ask for it: ' +
          'the plan will refuse to continue without this confirmation.</p>';
        html += '<label class="check"><input type="checkbox" id="setup-overwrite"' +
          (wizard.overwriteManifest ? ' checked' : '') +
          '> replace the existing manifest with the configuration above</label>';
      } else {
        html += '<p class="hint">GitMesh will write <span class="mono">' + escapeHtml(manifestPath) +
          '</span> — and nothing else: no file is moved, renamed or deleted.</p>';
      }
      html += '<ul class="facts">' +
        '<li>The file records the project name, the root repository and every external repository ' +
        'with its path and remote, using the existing project schema.</li>' +
        '<li>Published in the review step before it is written: nothing is generated behind your back.</li>' +
        '</ul>';
      node.innerHTML = html;
      if ($('setup-overwrite')) {
        $('setup-overwrite').addEventListener('change', function (event) {
          wizard.overwriteManifest = event.target.checked;
          invalidatePlan();
        });
      }
    }

    // ------------------------------------------------------------- the request --

    /// The answers, in the shape the client-side checks and the request builder use.
    function setupForm() {
      var root = rootRemoteForm();
      return {
        root: $('setup-root').value.trim(),
        name: $('setup-name').value.trim(),
        rootCreate: $('setup-root-create').checked,
        rootBranch: $('setup-root-branch').value.trim(),
        rootRemoteKind: root.remoteKind,
        rootRemote: root.remote,
        rootOwner: root.owner,
        rootName: root.name,
        rootScheme: root.scheme,
        rootVisibility: root.visibility,
        configureRemotes: $('setup-configure-remotes').checked,
        untrack: $('setup-untrack').checked,
        publish: $('setup-publish').checked,
        firstCommit: $('setup-first-commit').value.trim(),
        overwriteManifest: !!wizard.overwriteManifest,
        confirmRemotes: wizard.confirmRemotes === true,
        repositories: selectedPaths().map(function (path) {
          var form = wizard.forms[path];
          return {
            path: path,
            id: form.id.trim(),
            create: !!form.create,
            untrack: !!form.untrack,
            provider: form.remoteKind === 'github' ? 'github' : '',
            owner: form.owner,
            name: form.name,
            scheme: form.scheme,
            visibility: form.visibility,
            remote: form.remoteKind === 'existing' || form.remoteKind === 'manual'
              ? String(form.remote || '').trim()
              : ''
          };
        })
      };
    }

    /// The request body. Repository records keep their literal separators, their values are
    /// escaped: that is why the body is assembled by hand here.
    function setupBody(planId) {
      var form = setupForm();
      var fields = [];
      function add(key, value) {
        fields.push(key + '=' + encodeURIComponent(value === undefined || value === null ? '' : String(value)));
      }
      add('path', form.root);
      add('name', form.name);
      add('root', form.rootCreate ? 'yes' : 'no');
      add('rootBranch', form.rootBranch);
      if (form.rootRemoteKind === 'github') {
        add('rootProvider', 'github');
        add('rootOwner', form.rootOwner);
        add('rootName', form.rootName);
        add('rootScheme', form.rootScheme);
        add('rootVisibility', form.rootVisibility);
      } else if (form.rootRemoteKind === 'manual' || form.rootRemoteKind === 'existing') {
        add('rootRemote', form.rootRemote);
      }
      add('configureRemotes', form.configureRemotes ? 'yes' : 'no');
      add('untrack', form.untrack ? 'yes' : 'no');
      add('overwriteManifest', form.overwriteManifest ? 'yes' : 'no');
      add('confirmRemotes', form.confirmRemotes ? 'yes' : 'no');
      if (form.publish) {
        add('publish', 'yes');
        add('firstCommit', form.firstCommit);
      }
      if (planId) { add('planId', planId); }
      form.repositories.forEach(function (repo) {
        fields.push('repositories=' + GitMesh.repositoryRecord(repo));
      });
      return fields.join('&');
    }

    /// The project name is part of every suggested repository id (`MyProject/engine` →
    /// `myproject-engine`), and the suggestions come from the Rust side, so changing the
    /// name re-scans instead of guessing. Ids the user edited are kept by `rebuildForms`.
    function renameHints() {
      if (!$('setup-root').value.trim()) { return; }
      scanDirectory();
    }

    /// Any answer that changes invalidates the reviewed plan: a confirmation is bound to the
    /// plan the user actually saw.
    function invalidatePlan() {
      if (!wizard.plan) { return; }
      wizard.plan = null;
      $('setup-plan').innerHTML = '<p class="hint">The answers changed — review the plan again ' +
        'before creating the project.</p>';
      $('setup-plan-id').textContent = '—';
      $('setup-confirm').checked = false;
      $('btn-setup-apply').disabled = true;
    }

    // --------------------------------------------------------------- step 9 --

    function reviewPlan() {
      var problem = GitMesh.setupFormProblem(setupForm());
      if (problem) {
        $('setup-plan-error').textContent = problem;
        $('setup-plan-error').hidden = false;
        return;
      }
      $('setup-plan-error').hidden = true;
      $('setup-review-hint').textContent = 'planning…';
      api('/api/setup/plan', setupBody(null)).then(function (data) {
        wizard.plan = data.plan;
        $('setup-review-hint').textContent = '';
        renderReview();
      }).catch(function (error) {
        $('setup-review-hint').textContent = '';
        $('setup-plan-error').textContent = error.message;
        $('setup-plan-error').hidden = false;
        $('setup-plan').innerHTML = '';
      });
    }

    function renderReviewState() {
      $('btn-setup-apply').disabled = !(wizard.plan && wizard.plan.ready && $('setup-confirm').checked);
      if (!wizard.plan) { return; }
      $('setup-plan-id').textContent = wizard.plan.id + ' — ' + wizard.plan.summary;
    }

    function renderReview() {
      var plan = GitMesh.setupPlanSummary(wizard.plan);
      var html = '<p class="plan-summary">' + escapeHtml(plan.summary) + '</p>';
      html += '<ul class="facts">' +
        '<li>Project: <strong>' + escapeHtml(plan.name) + '</strong></li>' +
        '<li>Root: <strong class="mono">' + escapeHtml(plan.root) + '</strong></li>' +
        '<li>Manifest: <strong class="mono">' + escapeHtml(plan.manifestPath) + '</strong>' +
        (wizard.inspection && wizard.inspection.manifest.exists ? ' (replaced, as confirmed)' : ' (new)') +
        '</li>' +
        '<li>Steps: <strong>' + plan.planned + '</strong> will run, ' + plan.already +
        ' already in place, ' + plan.blocked + ' refused</li>' +
        '<li>Repositories created by this setup: <strong>' + plan.createdRepositories + '</strong></li>' +
        '</ul>';

      if (plan.blockers.length) {
        html += '<div class="result-section"><h3>Refused</h3><ul class="plan-blockers">';
        plan.blockers.forEach(function (blocker) { html += '<li>' + escapeHtml(blocker) + '</li>'; });
        html += '</ul><p class="hint">Correct the answers above and review again: GitMesh refuses ' +
          'instead of guessing.</p></div>';
      }
      if (plan.warnings.length) {
        html += '<div class="result-section"><h3>Warnings</h3><ul>';
        plan.warnings.forEach(function (line) { html += '<li>' + escapeHtml(line) + '</li>'; });
        html += '</ul></div>';
      }
      if (plan.notices.length) {
        html += '<div class="result-section"><h3>What GitMesh found</h3><ul>';
        plan.notices.forEach(function (line) { html += '<li>' + escapeHtml(line) + '</li>'; });
        html += '</ul></div>';
      }

      html += '<div class="result-section"><h3>Repositories</h3>' +
        '<table class="step-table"><thead><tr><th>Repository</th><th>Path</th><th>Role</th>' +
        '<th>What GitMesh will do</th></tr></thead><tbody>';
      plan.repositories.forEach(function (repo) {
        html += '<tr><td>' + escapeHtml(repo.id) + '</td><td class="owner mono">' + escapeHtml(repo.path) +
          '</td><td class="owner">' + escapeHtml(repo.role) + '</td><td>' + escapeHtml(repo.sentence) +
          '</td></tr>';
      });
      html += '</tbody></table></div>';

      html += '<div class="result-section"><h3>Steps</h3>' +
        '<table class="step-table"><thead><tr><th>Step</th><th>Target</th><th>State</th>' +
        '<th>Detail</th></tr></thead><tbody>';
      plan.steps.forEach(function (step) {
        html += '<tr><td>' + escapeHtml(step.heading) + '</td>' +
          '<td class="owner mono">' + escapeHtml(step.target) + '</td>' +
          '<td class="state-' + escapeHtml(step.state) + '">' + escapeHtml(GitMesh.stepStateLabel(step.state)) +
          (step.reason ? '<br><span class="owner">' + escapeHtml(step.reason) + '</span>' : '') +
          '</td><td>' + escapeHtml(step.detail) + '<br><span class="owner mono">' +
          escapeHtml(step.path) + '</span></td></tr>';
      });
      html += '</tbody></table></div>';

      var commands = GitMesh.hostedCommands(wizard.plan);
      if (commands.length) {
        html += '<div class="result-section"><h3>Hosted repositories</h3>';
        commands.forEach(function (entry) {
          html += '<p>' + escapeHtml(entry.id) + ' → <span class="mono">' +
            escapeHtml(entry.fullName) + '</span> (' + escapeHtml(entry.visibility) + ')</p>' +
            '<div class="command">' + escapeHtml(entry.command) + '</div>' +
            '<p class="hint">' + escapeHtml(entry.note) + '</p>';
        });
        html += '</div>';
      }

      if (plan.publish) {
        html += '<div class="result-section"><h3>After the setup</h3><p>' +
          escapeHtml(plan.publish.sentence) + '</p><p class="hint">' +
          escapeHtml(plan.publish.safety) + '</p></div>';
      }

      html += '<div class="result-section"><h3>Guarantees</h3><ul class="safety">';
      plan.safety.forEach(function (line) { html += '<li>✓ ' + escapeHtml(line) + '</li>'; });
      html += '</ul></div>';

      html += '<div class="result-section"><h3>The manifest GitMesh will write</h3>' +
        '<p class="hint">Exactly this file, generated from the answers above.</p>' +
        '<div class="manifest-preview">' + escapeHtml(plan.manifest) + '</div></div>';

      // A confirmation the plan itself asks for. Ticking it regenerates the plan, so what
      // runs is always what is displayed.
      var needsRemoteConfirmation = plan.replacesRemote.length > 0 || plan.steps.some(function (step) {
        return step.kind === 'configure-remote' && step.state === 'blocked';
      });
      if (needsRemoteConfirmation) {
        html += '<div class="result-section"><h3>Confirmation needed</h3>' +
          '<label class="check"><input type="checkbox" id="setup-confirm-remotes-plan"' +
          (wizard.confirmRemotes ? ' checked' : '') + '> replace the existing <code>origin</code> of ' +
          escapeHtml(plan.replacesRemote.map(function (repo) { return repo.id; }).join(', ') ||
            'the repositories listed above') + '</label>' +
          '<p class="hint">Changing this regenerates the plan above.</p></div>';
      }

      $('setup-plan').innerHTML = html;
      if ($('setup-confirm-remotes-plan')) {
        $('setup-confirm-remotes-plan').addEventListener('change', function (event) {
          wizard.confirmRemotes = event.target.checked;
          renderReviewState();
          reviewPlan();
        });
      }
      renderReviewState();
    }

    // ------------------------------------------------------------ steps 10-12 --

    function applyPlan() {
      if (!wizard.plan || !wizard.plan.ready || !$('setup-confirm').checked) { return; }
      $('setup-step-11').hidden = false;
      $('setup-step-12').hidden = true;
      $('setup-progress').innerHTML = '<div class="progress-row progress-running">' +
        '<span class="symbol">…</span><span class="name">starting</span></div>';
      api('/api/setup/apply', setupBody(wizard.plan.id)).then(function (started) {
        var events = [];
        var state = GitMesh.setupProgressRows(events, []);
        var source = new EventSource('/api/events/' + started.id);
        function finish(payload) {
          source.close();
          showSetupResult(payload, state.rows);
        }
        source.onmessage = function (message) {
          var event;
          try { event = JSON.parse(message.data); } catch (error) { return; }
          events.push(event);
          state = GitMesh.setupProgressRows(events, []);
          renderSetupProgress(state.rows);
          if (event.type === 'finished' || event.type === 'failed') { finish(event); }
        };
        source.addEventListener('result', function (message) {
          var payload;
          try { payload = JSON.parse(message.data); } catch (error) { return; }
          finish(payload);
        });
        source.addEventListener('closed', function () { source.close(); });
        source.onerror = function () {
          source.close();
          $('setup-result').innerHTML = '<p class="resolve">The progress stream ended before the ' +
            'setup reported a result. Nothing was deleted by the interrupted setup: scan the ' +
            'directory again to see what exists now.</p>';
        };
      }).catch(function (error) {
        // A stale plan id comes back with the plan that changed: show it, never run it.
        if (error.data && error.data.plan) {
          wizard.plan = error.data.plan;
          renderReview();
          $('setup-plan-error').textContent = error.message;
          $('setup-plan-error').hidden = false;
        } else {
          $('setup-progress').innerHTML = '<div class="progress-row progress-failed">' +
            '<span class="symbol">✗</span><span>' + escapeHtml(error.message) + '</span></div>';
        }
      });
    }

    function renderSetupProgress(rows) {
      var html = '';
      rows.forEach(function (row) {
        var label = row.label || row.id;
        html += '<div class="progress-row progress-' + row.status + '">' +
          '<span class="symbol">' + GitMesh.outcomeSymbol(row.status) + '</span>' +
          '<span class="name">' + escapeHtml(label) + '</span>' +
          '<span>' + escapeHtml(row.summary || row.detail || labelFor(row.status)) + '</span></div>';
      });
      $('setup-progress').innerHTML = html;
    }

    function showSetupResult(payload, rows) {
      var text = GitMesh.setupResultText(payload, rows);
      var publish = GitMesh.publishResultText(text.publish);
      $('setup-step-12').hidden = false;
      var html = '<p class="plan-summary">' + escapeHtml(text.sentence) + ' — ' +
        escapeHtml(text.summary) + '</p>';
      html += '<p class="owner mono">' + text.succeeded + ' done · ' + text.skipped +
        ' already or skipped · ' + text.failed + ' failed</p>';
      if (text.validation) {
        html += '<div class="result-section"><h3>Validation</h3><ul>' +
          '<li>' + (text.validationOk
            ? '<span class="result-ok">✓</span> the project loads through the normal discovery and ' +
              'manifest code'
            : '<span class="result-bad">✗</span> the project still has problems') + '</li>';
        (text.validation.repositories || []).forEach(function (repo) {
          html += '<li>' + (repo.ok ? '✓' : '✗') + ' ' + escapeHtml(repo.id) +
            ' <span class="owner mono">' + escapeHtml(repo.path) + '</span>' +
            (repo.branch ? ' · branch ' + escapeHtml(repo.branch) : '') +
            (repo.issues && repo.issues.length
              ? '<br><span class="result-bad">' + escapeHtml(repo.issues.join('; ')) + '</span>' : '') +
            '</li>';
        });
        text.validationIssues.forEach(function (issue) {
          html += '<li><span class="result-bad">✗</span> ' + escapeHtml(issue) + '</li>';
        });
        html += '</ul></div>';
      }
      if (publish) {
        html += '<div class="result-section"><h3>First commit and push</h3>' +
          '<p class="owner">' + publish.commits + ' commit result(s), ' + publish.pushes +
          ' push result(s) — ' + publish.succeeded + ' succeeded, ' + publish.skipped +
          ' skipped</p>';
        if (publish.problems.length) {
          publish.problems.forEach(function (problem) {
            html += '<p class="resolve">' + escapeHtml(problem) + '</p>';
          });
        } else {
          html += '<p class="owner">The repositories the plan listed were committed and published.</p>';
        }
        html += '</div>';
      }
      if (!text.opened) {
        html += '<p class="hint">The interface stayed here because the setup did not finish ' +
          'completely. Fix what failed, then review the plan again: GitMesh always reports what ' +
          'succeeded, what failed and what remains.</p>';
      }
      $('setup-result').innerHTML = html;
      $('btn-setup-open').hidden = !text.validation;

      if (text.opened && payload.model) {
        // Same interface, no restart: the project replaces the wizard, and the result stays
        // on screen in the operation panel.
        model = payload.model;
        wizard.active = false;
        $('setup').hidden = true;
        render();
        showOperation(rows, { operation: 'Project setup', dryRun: false });
        $('operation-result').hidden = false;
        $('operation-result').innerHTML = '<p class="summary">' + escapeHtml(text.sentence) + ' — ' +
          escapeHtml(text.summary) + '</p><p class="owner mono">' + escapeHtml(text.manifest) + '</p>';
        $('status-line').textContent = 'project created';
      }
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
    $('btn-setup').addEventListener('click', openWizard);
    $('btn-welcome-setup').addEventListener('click', function () {
      $('setup-root').value = (model && model.directory) || $('welcome-path').value || '';
      openWizard();
    });
    $('btn-setup-close').addEventListener('click', closeWizard);
    $('btn-setup-cancel').addEventListener('click', closeWizard);
    $('btn-setup-scan').addEventListener('click', scanDirectory);
    $('setup-root').addEventListener('keydown', function (event) {
      if (event.key === 'Enter') { event.preventDefault(); scanDirectory(); }
    });
    $('setup-name').addEventListener('change', renameHints);
    $('btn-setup-suggest').addEventListener('click', function () {
      var directories = GitMesh.selectableDirectories(wizard.inspection);
      directories.forEach(function (directory) {
        wizard.selection[directory.path] = !!directory.suggested;
      });
      rebuildForms();
      renderWizard();
      invalidatePlan();
    });
    $('btn-setup-clear').addEventListener('click', function () {
      Object.keys(wizard.selection).forEach(function (path) { wizard.selection[path] = false; });
      renderWizard();
      invalidatePlan();
    });
    $('setup-untrack').addEventListener('change', invalidatePlan);
    $('setup-configure-remotes').addEventListener('change', invalidatePlan);
    $('setup-publish').addEventListener('change', invalidatePlan);
    $('setup-first-commit').addEventListener('change', invalidatePlan);
    $('setup-root-remote-kind').addEventListener('change', function () {
      renderRootRepository();
      renderWizard();
      invalidatePlan();
    });
    $('btn-setup-review').addEventListener('click', reviewPlan);
    $('setup-confirm').addEventListener('change', renderReviewState);
    $('btn-setup-apply').addEventListener('click', applyPlan);
    $('btn-setup-again').addEventListener('click', function () {
      $('setup-step-12').hidden = true;
      reviewPlan();
    });
    $('btn-setup-open').addEventListener('click', function () {
      openProject($('setup-root').value.trim(), $('setup-scan-error'));
      setTimeout(function () { if (model && model.opened) { closeWizard(); } }, 400);
    });
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
