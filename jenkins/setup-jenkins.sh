#!/usr/bin/env bash

# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Stand up swervo's Jenkins job, job-as-code, mirroring NavGator/jenkins/setup-jenkins.sh.
#
# Env: JENKINS_URL (default http://localhost:8080), JENKINS_USER and JENKINS_TOKEN (your
#      Jenkins user and API token, both required), REPO_URL (default: this checkout's
#      `origin` remote).
# Needs: java + curl + git. The job is a multibranch pipeline over REPO_URL that builds every
# branch with the root Jenkinsfile (agent labels: linux, macos, windows).

set -o errexit
set -o nounset
set -o pipefail

JENKINS_URL="${JENKINS_URL:-http://localhost:8080}"
: "${JENKINS_USER:?set JENKINS_USER (your Jenkins user name)}"
: "${JENKINS_TOKEN:?set JENKINS_TOKEN (a Jenkins API token)}"

CLI="/tmp/jenkins-cli.jar"
if [[ ! -f "${CLI}" ]]; then
    curl -fsSL "${JENKINS_URL}/jnlpJars/jenkins-cli.jar" -o "${CLI}"
fi
DIR="$(cd "$(dirname "${0}")" && pwd)"
REPO_URL="${REPO_URL:-$(git -C "${DIR}" remote get-url origin)}"
auth=(-s "${JENKINS_URL}" -auth "${JENKINS_USER}:${JENKINS_TOKEN}")

java -jar "${CLI}" "${auth[@]}" who-am-i >/dev/null

job=swervo-ci
echo "  creating/updating ${job} ..."
config="$(sed "s|@REPO_URL@|${REPO_URL}|" "${DIR}/job-configs/${job}.xml")"
java -jar "${CLI}" "${auth[@]}" create-job "${job}" <<< "${config}" 2>/dev/null \
    || java -jar "${CLI}" "${auth[@]}" update-job "${job}" <<< "${config}"

echo "Done. Jenkins indexes the branches every 5 minutes; to start now:"
echo "  java -jar ${CLI} -s ${JENKINS_URL} -auth ${JENKINS_USER}:\${JENKINS_TOKEN} build ${job}/main"
