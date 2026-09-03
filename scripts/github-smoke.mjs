import { generateKeyPairSync } from 'node:crypto';
import { App, Octokit } from 'octokit';

const baseUrl = 'http://127.0.0.1:8097/api/v3';
const owner = 'mintlify';
const repo = 'editor-e2e';
const { privateKey } = generateKeyPairSync('rsa', {
  modulusLength: 2048,
  privateKeyEncoding: { type: 'pkcs1', format: 'pem' },
  publicKeyEncoding: { type: 'pkcs1', format: 'pem' },
});

const results = [];
const step = async (name, fn) => {
  try {
    const out = await fn();
    results.push([name, 'ok', out]);
  } catch (e) {
    results.push([name, 'FAIL', `${e.status ?? ''} ${e.message}`.trim()]);
  }
};

const app = new App({ appId: 1, privateKey, Octokit: Octokit.defaults({ baseUrl }) });
let octokit;
await step('app.getInstallationOctokit(1)', async () => {
  octokit = await app.getInstallationOctokit(1);
  const { data } = await octokit.rest.apps.getAuthenticated();
  return data.slug ?? data.name;
});
const user = new Octokit({ baseUrl, auth: 'gho_dev' });

await step('repos.get', async () => (await octokit.rest.repos.get({ owner, repo })).data.default_branch);
await step('repos.getBranch', async () => (await octokit.rest.repos.getBranch({ owner, repo, branch: 'main' })).data.commit.sha.slice(0, 7));
let headSha;
await step('repos.getCommit', async () => {
  const { data } = await octokit.rest.repos.getCommit({ owner, repo, ref: 'main' });
  headSha = data.sha;
  return `${data.commit.committer.date} parents=${data.parents.length}`;
});
await step('git.getTree recursive', async () => {
  const { data } = await octokit.rest.git.getTree({ owner, repo, tree_sha: 'main', recursive: '1' });
  return `${data.tree.length} entries truncated=${data.truncated}`;
});
await step('repos.getContent docs.json', async () => {
  const { data } = await octokit.rest.repos.getContent({ owner, repo, path: 'docs.json', ref: 'main' });
  const text = Buffer.from(data.content, data.encoding).toString();
  return `${data.encoding} ${text.length}b sha=${data.sha.slice(0, 7)}`;
});
await step('repos.getContent raw', async () => {
  const { data } = await octokit.request('GET /repos/{owner}/{repo}/contents/{path}', {
    owner, repo, path: 'docs.json', mediaType: { format: 'raw' },
  });
  return `${String(data).length}b`;
});
await step('repos.getContent dir', async () => {
  const { data } = await octokit.rest.repos.getContent({ owner, repo, path: '' });
  return `${data.length} entries`;
});
await step('git.getBlob', async () => {
  const { data: t } = await octokit.rest.git.getTree({ owner, repo, tree_sha: 'main' });
  const blob = t.tree.find((e) => e.type === 'blob');
  const { data } = await octokit.rest.git.getBlob({ owner, repo, file_sha: blob.sha });
  return `${blob.path} ${data.size}b ${data.encoding}`;
});
await step('collaborators permission', async () => (await octokit.rest.repos.getCollaboratorPermissionLevel({ owner, repo, username: 'x' })).data.permission);
await step('GET /user x-oauth-scopes', async () => {
  const r = await user.request('GET /user');
  return `${r.data.login} scopes=${r.headers['x-oauth-scopes']}`;
});
await step('rules/branches', async () => (await octokit.request('GET /repos/{owner}/{repo}/rules/branches/{branch}', { owner, repo, branch: 'main' })).data.length);

const branch = `smoke-${Date.now()}`;
await step('git.createRef', async () => (await user.rest.git.createRef({ owner, repo, ref: `refs/heads/${branch}`, sha: headSha })).data.ref);

const latestQuery = `query($owner:String!,$repo:String!,$ref:String!){repository(owner:$owner,name:$repo){ref(qualifiedName:$ref){target{... on Commit{history(first:1){nodes{oid}}}}}}}`;
await step('graphql latest commit', async () => {
  const d = await user.graphql(latestQuery, { owner, repo, ref: `refs/heads/${branch}` });
  return d.repository.ref.target.history.nodes[0].oid.slice(0, 7);
});
const mutation = `mutation($input:CreateCommitOnBranchInput!){createCommitOnBranch(input:$input){commit{oid url}}}`;
let newOid;
await step('graphql createCommitOnBranch', async () => {
  const d = await user.graphql(mutation, {
    input: {
      branch: { repositoryNameWithOwner: `${owner}/${repo}`, branchName: branch },
      expectedHeadOid: headSha,
      message: { headline: 'smoke: add page' },
      fileChanges: { additions: [{ path: 'smoke/page.mdx', contents: Buffer.from('# Smoke\n').toString('base64') }] },
    },
  });
  newOid = d.createCommitOnBranch.commit.oid;
  return newOid.slice(0, 7);
});
await step('graphql stale expectedHeadOid', async () => {
  try {
    await user.graphql(mutation, {
      input: {
        branch: { repositoryNameWithOwner: `${owner}/${repo}`, branchName: branch },
        expectedHeadOid: headSha,
        message: { headline: 'stale' },
        fileChanges: { additions: [{ path: 'x', contents: 'eA==' }] },
      },
    });
    return 'unexpected success';
  } catch (e) {
    return `${e.errors?.[0]?.type}: ${e.errors?.[0]?.message?.slice(0, 60)}`;
  }
});
await step('repos.compareCommits', async () => {
  const { data } = await octokit.rest.repos.compareCommits({ owner, repo, base: 'main', head: branch });
  return `${data.status} ahead=${data.ahead_by} files=${data.files.map((f) => `${f.status}:${f.filename}`).join(',')} mb=${data.merge_base_commit.sha.slice(0, 7)}`;
});
let pr;
await step('pulls.create', async () => {
  pr = (await user.rest.pulls.create({ owner, repo, title: 'smoke', head: branch, base: 'main', body: 'b' })).data;
  return `#${pr.number} ${pr.node_id}`;
});
await step('pulls.get', async () => {
  const { data } = await octokit.rest.pulls.get({ owner, repo, pull_number: pr.number });
  return `${data.state} mergeable=${data.mergeable} head=${data.head.sha.slice(0, 7)}`;
});
await step('pulls.listFiles', async () => (await octokit.rest.pulls.listFiles({ owner, repo, pull_number: pr.number })).data.map((f) => f.filename).join(','));
await step('pulls.list open', async () => (await octokit.rest.pulls.list({ owner, repo, state: 'open', head: `${owner}:${branch}` })).data.length);
await step('pulls.createReview APPROVE', async () => (await user.rest.pulls.createReview({ owner, repo, pull_number: pr.number, event: 'APPROVE', body: 'lgtm' })).data.state);
await step('issues.createComment', async () => (await user.rest.issues.createComment({ owner, repo, issue_number: pr.number, body: 'hi' })).data.id);
await step('pulls.merge', async () => (await user.rest.pulls.merge({ owner, repo, pull_number: pr.number, merge_method: 'merge' })).data);
await step('main advanced', async () => {
  const { data } = await octokit.rest.repos.getCommit({ owner, repo, ref: 'main' });
  return `${data.sha.slice(0, 7)} parents=${data.parents.length}`;
});
await step('commits/{sha}/pulls', async () => (await octokit.rest.repos.listPullRequestsAssociatedWithCommit({ owner, repo, commit_sha: newOid })).data.map((p) => p.number).join(','));
await step('git.deleteRef', async () => (await user.rest.git.deleteRef({ owner, repo, ref: `heads/${branch}` })).status);
await step('check-runs create+patch', async () => {
  const { data } = await octokit.rest.checks.create({ owner, repo, name: 'x', head_sha: newOid, status: 'in_progress' });
  await octokit.rest.checks.update({ owner, repo, check_run_id: data.id, conclusion: 'success', status: 'completed' });
  return data.id;
});

for (const [name, status, out] of results) console.log(`${status.padEnd(4)} ${name.padEnd(34)} ${typeof out === 'object' ? JSON.stringify(out) : out}`);
const failed = results.filter((r) => r[1] === 'FAIL').length;
console.log(`\n${results.length - failed}/${results.length} passed`);
process.exit(failed ? 1 : 0);
