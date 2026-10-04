const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');

module.exports = async ({ github, context, core }) => {
  const tag = context.ref.replace(/^refs\/tags\//, '');
  const version = JSON.parse(fs.readFileSync('package.json', 'utf8')).version;
  if (tag !== `v${version}`) throw new Error('Tag/version mismatch');
  const readme = fs.readFileSync('README.md', 'utf8').replace(/\r\n/g, '\n');
  const releaseBody = readme.split('\n## 发布说明\n')[1]?.split('\n## ')[0].trim();
  if (!releaseBody) throw new Error('README release notes section is missing or empty');
  const directory = 'release-installers';
  const files = fs.readdirSync(directory).filter((name) => name !== 'SHA256SUMS').sort();
  const expected = ['windows-x64.exe', 'macos-arm64.dmg', 'macos-x64.dmg', 'linux-x64.deb', 'linux-x64.AppImage']
    .map((suffix) => `codex-auto-resume_${version}_${suffix}`).sort();
  if (JSON.stringify(files) !== JSON.stringify(expected)) throw new Error('Installer set is incomplete or unexpected');
  const checksums = files.map((name) => `${crypto.createHash('sha256').update(fs.readFileSync(path.join(directory, name))).digest('hex')}  ${name}`).join('\n') + '\n';
  fs.writeFileSync(path.join(directory, 'SHA256SUMS'), checksums);
  const repo = context.repo;
  let release;
  // Drafts are not reliably exposed by the get-by-tag endpoint; list with auth.
  const releases = await github.paginate(github.rest.repos.listReleases, { ...repo, per_page: 100 });
  release = releases.find((item) => item.tag_name === tag);
  if (release && !release.draft) throw new Error('Existing release is public; refusing to overwrite it');
  if (!release) {
    release = (await github.rest.repos.createRelease({
      ...repo, tag_name: tag, target_commitish: context.sha,
      name: `Codex Auto Resume ${tag}`, draft: true, prerelease: true,
      body: releaseBody,
    })).data;
  }
  const assets = await github.paginate(github.rest.repos.listReleaseAssets, { ...repo, release_id: release.id, per_page: 100 });
  const allowed = new Set([...expected, 'SHA256SUMS']);
  if (assets.some((asset) => !allowed.has(asset.name))) {
    throw new Error('Draft contains unexpected assets; review and remove them manually before retrying');
  }
  for (const name of [...files, 'SHA256SUMS']) {
    const existing = assets.find((asset) => asset.name === name);
    if (existing) await github.rest.repos.deleteReleaseAsset({ ...repo, asset_id: existing.id });
    await github.rest.repos.uploadReleaseAsset({
      ...repo, release_id: release.id, name,
      data: fs.readFileSync(path.join(directory, name)),
      headers: { 'content-type': 'application/octet-stream' },
    });
  }
  await core.summary.addHeading('Draft prerelease ready').addLink('Review release', release.html_url).write();
};
