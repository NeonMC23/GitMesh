// Tests for the pure client logic of the GitMesh interface.
//
// This file is not shipped to the browser: `tests/client.rs` concatenates it with the
// region of app.js between the clientLogic markers and runs the result with Node. That
// keeps the browser code and the tested code the same text.

var failures = [];
var assertions = 0;

function assert(condition, message) {
  assertions++;
  if (!condition) { failures.push(message); }
}

function equal(actual, expected, message) {
  assertions++;
  if (actual !== expected) {
    failures.push(message + ' (expected ' + JSON.stringify(expected) +
      ', got ' + JSON.stringify(actual) + ')');
  }
}

function contains(haystack, needle, message) {
  assertions++;
  if (String(haystack).indexOf(needle) === -1) {
    failures.push(message + ' (did not find ' + JSON.stringify(needle) + ')');
  }
}

// ---------------------------------------------------------------- fixtures --

function sampleModel(overrides) {
  var model = {
    kind: 'project',
    opened: true,
    dryRun: false,
    project: {
      name: 'MyProject',
      root: '/home/dev/MyProject',
      manifest: '/home/dev/MyProject/.gitmesh/project.toml',
      state: { key: 'changed', label: 'modified', needsAttention: false },
      branch: { name: 'main', consistent: true, outliers: [] },
      counts: {
        repositories: 4, changed: 2, clean: 2, conflicted: 0,
        unavailable: 0, changes: 3
      },
      notices: []
    },
    repositories: [
      {
        id: 'root', role: 'root', path: '.', branch: 'main', head: 'main',
        state: { key: 'changed', label: 'modified' },
        counts: { staged: 0, unstaged: 1, untracked: 1, conflict: 0 },
        sync: { upstream: 'origin/main', ahead: 1, behind: 0 },
        remote: '/tmp/remotes/root.git', summary: 'main, 1 modified, 1 untracked'
      },
      {
        id: 'engine', role: 'external', path: 'engine', branch: 'main', head: 'main',
        state: { key: 'clean', label: 'clean' },
        counts: { staged: 0, unstaged: 0, untracked: 0, conflict: 0 },
        sync: { upstream: 'origin/main', ahead: 0, behind: 0 },
        remote: 'git@github.com:acme/engine.git'
      },
      {
        id: 'renderer', role: 'external', path: 'renderer', branch: 'main', head: 'main',
        state: { key: 'conflicted', label: 'conflicted' },
        counts: { staged: 1, unstaged: 0, untracked: 0, conflict: 1 },
        sync: { upstream: null, ahead: 0, behind: 2 },
        remote: null
      },
      {
        id: 'tools', role: 'external', path: 'tools', branch: 'main', head: 'main',
        state: { key: 'clean', label: 'clean' },
        counts: { staged: 0, unstaged: 0, untracked: 0, conflict: 0 },
        sync: { upstream: 'origin/main', ahead: 0, behind: 0 }
      }
    ],
    changes: [
      {
        path: 'src/main.rs', relativePath: 'src/main.rs', repository: 'root',
        repositoryPath: '.', type: { key: 'modified', label: 'modified', blocksCommit: false },
        staged: false, unstaged: true, untracked: false, conflict: false, originalPath: null
      },
      {
        path: 'engine/src/lib.rs', relativePath: 'src/lib.rs', repository: 'engine',
        repositoryPath: 'engine', type: { key: 'untracked', label: 'untracked', blocksCommit: false },
        staged: false, unstaged: false, untracked: true, conflict: false, originalPath: null
      },
      {
        path: 'renderer/src/index.js', relativePath: 'src/index.js', repository: 'renderer',
        repositoryPath: 'renderer', type: { key: 'conflict', label: 'conflicted', blocksCommit: true },
        staged: true, unstaged: true, untracked: false, conflict: true, originalPath: null
      },
      {
        path: 'tools/old.sh', relativePath: 'old.sh', repository: 'tools',
        repositoryPath: 'tools', type: { key: 'renamed', label: 'renamed', blocksCommit: false },
        staged: true, unstaged: false, untracked: false, conflict: false, originalPath: 'new.sh'
      }
    ],
    tree: {
      name: 'MyProject', path: '.', repository: 'root', isExternalRepository: false,
      files: 2, truncated: false,
      children: [
        {
          name: 'src', path: 'src', repository: 'root', isExternalRepository: false,
          files: 5, truncated: false, children: []
        },
        {
          name: 'engine', path: 'engine', repository: 'engine', isExternalRepository: true,
          files: 1, truncated: false, children: []
        }
      ]
    },
    branches: {
      list: [
        { name: 'main', presentIn: ['root', 'engine', 'renderer', 'tools'],
          checkedOutIn: ['root', 'engine', 'renderer', 'tools'], everywhere: true },
        { name: 'feature/gpu', presentIn: ['root', 'engine'], checkedOutIn: [], everywhere: false }
      ],
      notices: []
    },
    readiness: {
      commit: {
        enabled: true, reason: null, files: 3,
        repositories: [
          { id: 'root', path: '.', files: 1 },
          { id: 'engine', path: 'engine', files: 1 },
          { id: 'tools', path: 'tools', files: 1 }
        ],
        blocked: [{ id: 'renderer', path: 'renderer', conflicts: 1, state: 'conflicted' }]
      },
      push: { enabled: true, reason: null, aheadIn: ['root'] },
      branch: { enabled: true, reason: null }
    },
    staging: { note: 'GitMesh commits every change of the project, staged or not.' }
  };
  if (overrides) {
    Object.keys(overrides).forEach(function (key) { model[key] = overrides[key]; });
  }
  return model;
}

function emptyModel() {
  return {
    kind: 'no-project', opened: false, directory: '/tmp/nowhere',
    error: "no GitMesh project found at or above /tmp/nowhere (expected a `.gitmesh/project.toml` manifest; run `gitmesh init` to create one)",
    configuration: true, tree: null
  };
}

// ---------------------------------------------------------------- project --

(function projectSummaryTests() {
  var summary = GitMesh.projectSummary(sampleModel());
  equal(summary.name, 'MyProject', 'the project name is shown');
  equal(summary.repositoryCount, 4, 'repositories are counted');
  equal(summary.changeCount, 3, 'changes are counted');
  equal(summary.branch, 'main', 'the logical branch is reported');
  equal(summary.branchConsistent, true, 'a consistent branch has no warning');
  equal(summary.state, 'changed', 'the project state comes from the core');
  equal(summary.dryRun, false, 'dry run is off by default');

  var inconsistent = sampleModel();
  inconsistent.project.branch = { name: 'main', consistent: false, outliers: ['renderer'] };
  var warning = GitMesh.branchWarning(inconsistent);
  contains(warning, 'renderer', 'the outlier repository is named');
  contains(warning, 'different branches', 'the split is stated');
  equal(GitMesh.branchWarning(sampleModel()), null, 'no warning when consistent');

  var notices = GitMesh.notices(sampleModel());
  equal(notices.length, 0, 'a healthy project has no notices');

  var unavailable = sampleModel();
  unavailable.project.counts.unavailable = 2;
  var withNotices = GitMesh.notices(unavailable);
  contains(withNotices.join(' '), '2 repositories', 'unavailable repositories are surfaced');

  equal(GitMesh.projectSummary(emptyModel()).opened, false, 'an empty model is not open');
})();

// ------------------------------------------------------------------- tree --

(function treeTests() {
  var lines = GitMesh.treeLines(sampleModel().tree);
  equal(lines.length, 3, 'the tree is flattened with its children');
  equal(lines[0].depth, 0, 'the root is depth 0');
  equal(lines[0].name, 'MyProject', 'the root line is the project itself');
  equal(lines[1].name, 'src', 'children follow their parent');
  equal(lines[1].depth, 1, 'children are one level deeper');
  equal(lines[2].path, 'engine', 'the external repository keeps its project path');
  equal(lines[2].external, true, 'external repositories are marked, not promoted');
  equal(GitMesh.treeLines(null).length, 0, 'a missing tree renders nothing');
})();

// ---------------------------------------------------------------- changes --

(function changeTests() {
  var model = sampleModel();
  var described = GitMesh.describeChange(model.changes[0]);
  equal(described.path, 'src/main.rs', 'paths are project-relative');
  equal(described.repository, 'root', 'the owning repository is known');
  equal(described.staging, 'not staged', 'staging state is described');

  var conflict = GitMesh.describeChange(model.changes[2]);
  equal(conflict.conflict, true, 'conflicts are flagged');
  equal(conflict.blocksCommit, true, 'a conflict blocks the commit');
  equal(conflict.staging, 'conflict', 'a conflicted file is described as such');

  var renamed = GitMesh.describeChange(model.changes[3]);
  equal(renamed.wasPath, 'new.sh', 'renames keep the original path');
  equal(renamed.typeLabel, 'renamed', 'the change type comes from the core');

  var counts = GitMesh.changeCounts(model.changes);
  equal(counts.total, 4, 'all changes are counted');
  equal(counts.conflicts, 1, 'conflicts are counted separately');
  equal(counts.untracked, 1, 'untracked files are counted');
  equal(counts.staged, 1, 'staged changes are counted (a staged rename counts as staged)');
  equal(counts.other, 1, 'unstaged modifications are counted as other');

  var mixed = GitMesh.changeCounts([
    { type: { key: 'staged_and_modified' }, conflict: false, untracked: false, staged: true, unstaged: true },
    { type: { key: 'deleted' }, conflict: false, untracked: false, staged: false, unstaged: true }
  ]);
  equal(mixed.staged, 1, 'staged and modified counts as staged');
  equal(mixed.other, 1, 'a deleted file counts as other');

  var groups = GitMesh.groupChangesByRepository(model.changes);
  equal(groups.length, 4, 'each repository owns one group here');
  equal(groups[0].id, 'root', 'groups keep the project order');
  equal(groups[2].conflicts, 1, 'a group reports its conflicts');
  equal(groups[2].path, 'renderer', 'a group carries the repository path');

  // The logical path is what the user sees: engine/src/lib.rs is one project file.
  var engine = GitMesh.describeChange(model.changes[1]);
  equal(engine.path, 'engine/src/lib.rs', 'the logical path is shown for external files');
  equal(engine.relativePath, 'src/lib.rs', 'the repository-relative path is kept for detail');
})();

// ----------------------------------------------------------------- commit --

(function commitTests() {
  var plan = GitMesh.commitPlan(sampleModel());
  equal(plan.enabled, true, 'commit is possible when nothing blocks it');
  equal(plan.repositories.length, 3, 'one target per affected repository');
  equal(plan.files, 3, 'the file count is summed');
  equal(plan.blocked.length, 1, 'the conflicted repository is listed as blocked');
  equal(plan.blocked[0].id, 'renderer', 'the blocked repository is named');

  assert(GitMesh.commitMessageProblem('') !== null, 'an empty message is refused');
  assert(GitMesh.commitMessageProblem('   ') !== null, 'a blank message is refused');
  equal(GitMesh.commitMessageProblem('real message'), null, 'a real message is accepted');

  var nothing = sampleModel();
  nothing.readiness.commit = {
    enabled: false, reason: 'every repository is clean', files: 0,
    repositories: [], blocked: []
  };
  var emptyPlan = GitMesh.commitPlan(nothing);
  equal(emptyPlan.enabled, false, 'nothing to commit disables the button');
  contains(emptyPlan.reason, 'clean', 'and explains why');
})();

// --------------------------------------------------------------- progress --

(function progressTests() {
  var events = [
    {
      type: 'started', operation: 'pull', sentence: 'Pulling the project', dryRun: false,
      at: 0, total: 3,
      repositories: [
        { id: 'root', path: '.', role: 'root' },
        { id: 'engine', path: 'engine', role: 'external' },
        { id: 'renderer', path: 'renderer', role: 'external' }
      ]
    },
    { type: 'repository', phase: 'running', id: 'root', path: '.', at: 5 },
    { type: 'outcome', id: 'root', path: '.', outcome: 'success', summary: 'already up to date', details: [], at: 40 },
    { type: 'repository', phase: 'running', id: 'engine', path: 'engine', at: 41 },
    { type: 'outcome', id: 'engine', path: 'engine', outcome: 'success', summary: 'pulled 2 commit(s)', details: [], at: 90 },
    { type: 'repository', phase: 'running', id: 'renderer', path: 'renderer', at: 91 },
    { type: 'outcome', id: 'renderer', path: 'renderer', outcome: 'conflict', summary: 'pull produced conflicts', details: ['src/index.js'], at: 150 },
    { type: 'finished', operation: 'pull', kind: 'partial', exitCode: 1, at: 151 }
  ];
  var state = GitMesh.progressRows(events, []);
  equal(state.rows.length, 3, 'every repository gets a row');
  equal(state.operation, 'pull', 'the operation name is known');
  equal(state.rows[0].status, 'success', 'finished repositories keep their outcome');
  equal(state.rows[1].summary, 'pulled 2 commit(s)', 'summaries come from the core');
  equal(state.rows[2].status, 'conflict', 'conflicts are visible in progress');
  equal(state.rows[2].details[0], 'src/index.js', 'conflicted files are listed');
  equal(state.finished, true, 'the operation is complete');

  // Mid-flight state: the running repository is marked, later ones are pending.
  var running = GitMesh.progressRows(events.slice(0, 4), []);
  equal(running.rows[0].status, 'success', 'completed repositories are success');
  equal(running.rows[1].status, 'running', 'the current repository is running');
  equal(running.rows[2].status, 'pending', 'later repositories are pending');
  equal(running.finished, false, 'an unfinished operation stays open');

  // A finished operation never leaves a repository spinning.
  var interrupted = GitMesh.progressRows(events.slice(0, 4).concat([{ type: 'finished', operation: 'pull' }]), []);
  assert(interrupted.rows.every(function (row) { return row.status !== 'running'; }),
    'no row stays "running" after the operation ends');

  var failed = GitMesh.progressRows([{ type: 'failed', message: 'boom' }], []);
  equal(failed.failed, 'boom', 'a refused operation carries its message');

  equal(GitMesh.outcomeSymbol('success'), '✓', 'symbols match the CLI');
  equal(GitMesh.outcomeSymbol('skipped'), '–', 'skipped uses the dash');
  equal(GitMesh.outcomeSymbol('conflict'), '!', 'conflicts use the exclamation mark');
  equal(GitMesh.outcomeSymbol('failed'), '✗', 'failures use the cross');
  equal(GitMesh.outcomeSymbol('running'), '…', 'a running repository shows a spinner');

  equal(GitMesh.resultTitle('pull', state.rows, false), 'Pull partly completed',
    'partial results say so');
  var allGood = [{ status: 'success' }, { status: 'skipped' }];
  equal(GitMesh.resultTitle('push', allGood, false), 'Push completed', 'clean runs say completed');
  equal(GitMesh.resultTitle('push', allGood, true), 'Push (dry run: nothing was changed)',
    'dry runs are labelled');
  equal(GitMesh.resultTitle('pull', [{ status: 'failed' }, { status: 'failed' }], false),
    'Pull failed', 'total failure says failed');

  var guidance = GitMesh.conflictGuidance(state.rows);
  assert(guidance !== null, 'conflicts come with guidance');
  contains(guidance.message, 'never resolves or discards', 'the guidance is honest');
  equal(guidance.repositories[0], 'renderer', 'the conflicted repository is named');
  contains(guidance.steps.join(' '), 'git add', 'the recovery commands are shown');
  equal(GitMesh.conflictGuidance(allGood), null, 'no guidance without conflicts');
})();

// ------------------------------------------------------------------- sync --

(function syncTests() {
  var summary = GitMesh.pushSummary(sampleModel());
  equal(summary.rows.length, 4, 'every repository is listed');
  equal(summary.pushable, 1, 'only repositories with unpublished commits can push');
  equal(summary.behind.length, 1, 'repositories behind their upstream are flagged');
  equal(summary.withoutUpstream[0], 'renderer', 'missing upstreams are listed');
})();

// ----------------------------------------------------------------- detail --

(function detailTests() {
  var model = sampleModel();
  contains(GitMesh.repositoryDetails(model.repositories[0]), '1 modified file',
    'modifications are described');
  contains(GitMesh.repositoryDetails(model.repositories[0]), 'ahead 1', 'ahead counts are shown');
  contains(GitMesh.repositoryDetails(model.repositories[2]), '1 conflict', 'conflicts are described');
  contains(GitMesh.repositoryDetails(model.repositories[1]), 'nothing to record',
    'clean repositories say so');

  var rows = GitMesh.settingsRows(model);
  contains(JSON.stringify(rows), 'MyProject', 'settings show the project name');
  contains(JSON.stringify(rows), 'project.toml', 'settings show the manifest location');
  contains(JSON.stringify(rows), 'no remote', 'missing remotes are stated');
  contains(JSON.stringify(rows), 'project root', 'the root repository is labelled');
})();

// ----------------------------------------------------------------- result --

if (failures.length) {
  console.error('client logic: ' + failures.length + ' of ' + assertions + ' assertions failed');
  failures.forEach(function (failure) { console.error('  - ' + failure); });
  if (typeof process !== 'undefined') { process.exit(1); }
} else {
  console.log('client logic: ' + assertions + ' assertions passed');
}
