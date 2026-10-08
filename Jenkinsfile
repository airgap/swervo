// swervo CI on Jenkins — replaces the GitHub Actions `Main`, `Lint`, `AVIF dependency` and
// `AVIF cross-builds` workflows (the only ones that do anything in this fork; the rest are
// upstream servo/servo plumbing: bencher, codecov, WPT import/export, nightly uploads, docs).
//
// Runs as a multibranch job (jenkins/job-configs/swervo-ci.xml), so every pushed branch is
// built. What runs mirrors python/servo/try_parser.py's presets as main.yml used them:
//   * every branch (the old pull_request run): Lint + Linux build with unit tests
//   * `main`, or FULL=true (the old push-to-main run): + macOS, Windows, Android
// Agents are the ones navgator's Jenkinsfile already uses: labels `linux`, `macos`, `windows`.
// Linux and Lint are the required gate; the other platforms are non-blocking (UNSTABLE, not
// FAILURE), the same "UNSTABLE until green, then required" convention as navgator.
//
// Agents are expected to be provisioned like navgator's (Rust via rustup, LLVM/clang, Python
// 3.11, sccache optional). Tick BOOTSTRAP to have `./mach bootstrap` install the system
// packages on a fresh agent (needs passwordless sudo on Linux).

pipeline {
    agent none

    parameters {
        choice(name: 'PROFILE', choices: ['checked-release', 'release', 'production', 'debug'],
               description: 'Cargo profile for the platform builds')
        booleanParam(name: 'FULL', defaultValue: false,
                     description: 'Also build macOS, Windows and Android (always on for main)')
        booleanParam(name: 'WPT', defaultValue: false,
                     description: 'Run the full Linux WPT suite after the Linux build (slow)')
        string(name: 'WPT_ARGS', defaultValue: '', description: 'Extra arguments for ./mach test-wpt')
        booleanParam(name: 'BOOTSTRAP', defaultValue: false,
                     description: 'Run ./mach bootstrap on each agent first (fresh agents)')
    }

    options {
        timestamps()
        ansiColor('xterm')
        buildDiscarder(logRotator(numToKeepStr: '20', artifactNumToKeepStr: '5'))
        timeout(time: 240, unit: 'MINUTES')
        // A new push to the same branch supersedes the running build (main.yml's concurrency).
        disableConcurrentBuilds(abortPrevious: true)
    }

    environment {
        CARGO_INCREMENTAL = '0'
        CARGO_TERM_COLOR = 'always'
        RUST_BACKTRACE = '1'
    }

    stages {
        stage('CI') {
            parallel {
                stage('Lint') {
                    agent { label 'linux' }
                    steps {
                        // mach test-tidy compares against the merge base, so it needs full history.
                        sh 'git fetch --unshallow --quiet 2>/dev/null || true; git fetch --quiet origin main || true'
                        prepareUnix(lint: true)
                        unix './mach clippy --locked -- -- --deny warnings'
                        unix './mach test-tidy --no-progress --all'
                    }
                }

                stage('Linux') {
                    agent { label 'linux' }
                    steps {
                        prepareUnix(nextest: true)
                        unix """
                            ${rustflags()} ./mach build --use-crown --locked --profile ${params.PROFILE}
                            rm -rf target/cargo-timings-linux && mv target/cargo-timings target/cargo-timings-linux || true
                        """
                        // Fork-specific regression tests (see linux.yml).
                        unix """
                            ./mach test-wpt --profile ${params.PROFILE} --processes 1 --timeout-multiplier 2 \\
                              --log-raw target/selection-modify-wpt.log \\
                              --log-wptreport target/selection-modify-wptreport.json \\
                              /_mozilla/mozilla/selection-modify-discarded-document.html
                            ./mach test-wpt --profile ${params.PROFILE} --processes 1 --timeout-multiplier 2 \\
                              --log-raw target/clipboard-wpt.log \\
                              --log-wptreport target/clipboard-wptreport.json \\
                              /_mozilla/input-events/input-events-textarea-cut-paste.html
                        """
                        unix "xvfb-run -a ./mach smoketest --profile ${params.PROFILE}"
                        unix './mach test-scripts'
                        unix """
                            export NEXTEST_RETRIES=2 LANG=en-US
                            ./mach test-unit --profile ${params.PROFILE} --nextest-profile ci
                            ./mach test-unit --profile ${params.PROFILE} --doc
                        """
                        unix "./mach package --profile ${params.PROFILE}"
                        script {
                            if (params.WPT) {
                                // WPT_ARGS reaches the shell as an environment variable (Jenkins exports
                                // parameters) and is split into words, never evaluated as shell code.
                                unix """
                                    mkdir -p wpt-logs/linux
                                    read -r -a wpt_args <<< "\${WPT_ARGS:-}"
                                    ./mach test-wpt --bin servo/servoshell --profile ${params.PROFILE} \\
                                      --processes \$(nproc) --timeout-multiplier 2 \\
                                      --log-raw wpt-logs/linux/raw.log \\
                                      --log-wptreport wpt-logs/linux/wptreport.json \\
                                      --log-raw-stable-unexpected wpt-logs/linux/unexpected.log \\
                                      \${wpt_args[@]+"\${wpt_args[@]}"}
                                """
                            }
                        }
                    }
                    post {
                        always {
                            junit allowEmptyResults: true, testResults: 'target/nextest/ci/junit.xml'
                            archiveArtifacts allowEmptyArchive: true, artifacts: [
                                'target/*-wpt.log', 'target/*-wptreport.json',
                                'target/cargo-timings-*/**', 'wpt-logs/**',
                                "target/${params.PROFILE}/servo-tech-demo.tar.gz",
                            ].join(',')
                        }
                    }
                }

                // The old `AVIF dependency` workflow, on every build since it is cheap: the dav1d
                // setup script's unit tests plus a check that dav1d-sys links the static decoder.
                stage('AVIF dependency') {
                    agent { label 'linux' }
                    steps {
                        prepareUnix(dav1dOnly: true)
                        unix 'python3 -m unittest discover -s etc/ci -p setup_dav1d_tests.py'
                        unix 'bash jenkins/dav1d-rust-smoke.sh'
                    }
                }

                stage('macOS') {
                    when {
                        beforeAgent true
                        expression { fullRun() }
                    }
                    agent { label 'macos' }
                    steps {
                        catchError(buildResult: 'UNSTABLE', stageResult: 'UNSTABLE') {
                            // XProtect can flag the freshly built DMG as malware (mac-arm64.yml).
                            sh 'sudo -n pkill -9 XProtect >/dev/null 2>&1 || true'
                            prepareUnix(nextest: true)
                            unix """
                                ${rustflags()} ./mach build --use-crown --locked --profile ${params.PROFILE}
                                rm -rf target/cargo-timings-macos && mv target/cargo-timings target/cargo-timings-macos || true
                            """
                            retry(2) { timeout(time: 5, unit: 'MINUTES') { unix "./mach smoketest --profile ${params.PROFILE}" } }
                            unix './mach test-scripts'
                            unix "NEXTEST_RETRIES=3 ./mach test-unit --profile ${params.PROFILE} --nextest-profile ci"
                            unix "./mach package --profile ${params.PROFILE}"
                            retry(2) {
                                timeout(time: 5, unit: 'MINUTES') {
                                    unix "./etc/ci/macos_package_smoketest.sh target/${params.PROFILE}/servo-tech-demo.dmg"
                                }
                            }
                        }
                    }
                    post {
                        always {
                            junit allowEmptyResults: true, testResults: 'target/nextest/ci/junit.xml'
                            archiveArtifacts allowEmptyArchive: true,
                                artifacts: "target/cargo-timings-*/**,target/${params.PROFILE}/servo-tech-demo.dmg"
                        }
                    }
                }

                stage('Windows') {
                    when {
                        beforeAgent true
                        expression { fullRun() }
                    }
                    agent { label 'windows' }
                    environment {
                        // clang-sys searches msys before Program Files\LLVM (windows.yml).
                        LIBCLANG_PATH = 'C:\\Program Files\\LLVM\\bin'
                        RUSTUP_WINDOWS_PATH_ADD_BIN = '1'
                    }
                    steps {
                        catchError(buildResult: 'UNSTABLE', stageResult: 'UNSTABLE') {
                            script {
                                if (params.BOOTSTRAP) {
                                    bat '.\\mach fetch && .\\mach bootstrap-gstreamer'
                                }
                            }
                            bat 'cargo install --path support\\crown --force'
                            bat 'where cargo-nextest >nul 2>nul || cargo install cargo-nextest --locked'
                            bat 'python jenkins\\ci_setup.py --env-file .ci-env'
                            win "${winRustflags()} && .\\mach build --use-crown --locked --profile ${params.PROFILE}"
                            win "${winRustflags()} && .\\mach build --use-crown --locked --profile ${params.PROFILE} --feature vello"
                            win ".\\mach smoketest --profile ${params.PROFILE}"
                            win "set NEXTEST_RETRIES=2&& .\\mach test-unit --profile ${params.PROFILE} --nextest-profile ci"
                            win ".\\mach package --profile ${params.PROFILE}"
                        }
                    }
                    post {
                        always {
                            junit allowEmptyResults: true, testResults: 'target/nextest/ci/junit.xml'
                            archiveArtifacts allowEmptyArchive: true,
                                artifacts: "target/${params.PROFILE}/msi/*.exe,target/${params.PROFILE}/*.zip"
                        }
                    }
                }

                // The old `AVIF cross-builds` / android.yml, for the arm64 target navgator ships.
                // Needs the Android SDK/NDK on the agent (ANDROID_SDK_ROOT, ANDROID_NDK_ROOT).
                // Release signing is not wired up; mach produces a debug-signed APK.
                stage('Android') {
                    when {
                        beforeAgent true
                        expression { fullRun() }
                    }
                    agent { label 'linux' }
                    steps {
                        catchError(buildResult: 'UNSTABLE', stageResult: 'UNSTABLE') {
                            prepareUnix(target: 'aarch64-linux-android')
                            unix """
                                : "\${ANDROID_NDK_ROOT:?set ANDROID_NDK_ROOT on the agent}"
                                ./mach build --use-crown --locked --target aarch64-linux-android --profile ${params.PROFILE}
                            """
                        }
                    }
                    post {
                        always {
                            archiveArtifacts allowEmptyArchive: true,
                                artifacts: "target/android/aarch64-linux-android/${params.PROFILE}/*.apk"
                        }
                    }
                }
            }
        }
    }

    post {
        success { echo 'swervo CI passed' }
        unstable { echo 'swervo CI unstable (a non-blocking platform failed)' }
        failure { echo 'swervo CI failed' }
    }
}

boolean fullRun() {
    return params.FULL || env.BRANCH_NAME == 'main'
}

// checked-release builds deny warnings, as in the GitHub workflows.
String rustflags() {
    return params.PROFILE == 'checked-release' ? 'RUSTFLAGS="-D warnings"' : ''
}

String winRustflags() {
    return params.PROFILE == 'checked-release' ? 'set "RUSTFLAGS=-D warnings"' : 'set "RUSTFLAGS="'
}

// A Unix shell step with the CI environment (toolchains on PATH, dav1d's .ci-env) loaded.
void unix(String script) {
    sh "#!/usr/bin/env bash\nsource jenkins/ci-env.sh\n${script}"
}

// A Windows batch step with .ci-env loaded (the GITHUB_ENV replacement).
void win(String script) {
    bat """
        @echo off
        if exist .ci-env for /f "usebackq tokens=1,* delims==" %%a in (".ci-env") do set "%%a=%%b"
        @echo on
        ${script}
    """
}

// Install what the GitHub workflows got from setup actions: rustup's pinned toolchain, uv,
// crown, nextest / lint tools, and the static dav1d build (.github/actions/setup-dav1d).
void prepareUnix(Map opts = [:]) {
    def target = opts.target ?: 'native'
    def bootstrapFlags = opts.lint ? '--skip-nextest' : '--skip-lints --skip-nextest'
    // Start from a clean env file so the PATH it records doesn't grow build over build.
    sh 'rm -f .ci-env'
    unix """
        command -v rustup >/dev/null || \\
          curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path --default-toolchain none
        export PATH="\$HOME/.cargo/bin:\$PATH"
        rustup show active-toolchain || rustup show
        command -v uv >/dev/null || curl -LsSf https://astral.sh/uv/0.11.16/install.sh | sh
        if command -v brew >/dev/null; then
            for t in nasm pkg-config gnu-tar; do brew list "\$t" >/dev/null 2>&1 || brew install "\$t"; done
        elif ! command -v nasm >/dev/null || ! command -v pkg-config >/dev/null; then
            sudo -n apt-get install -y nasm pkg-config
        fi
    """
    if (params.BOOTSTRAP && !opts.dav1dOnly) {
        unix "./mach bootstrap --yes ${bootstrapFlags}"
    }
    if (!opts.dav1dOnly) {
        // Always reinstall crown: it is tied to the rustc version of the commit being built.
        unix 'cargo install --path support/crown'
    }
    if (opts.nextest) {
        unix 'command -v cargo-nextest >/dev/null || cargo install cargo-nextest --locked'
    }
    if (opts.lint) {
        unix '''
            command -v taplo >/dev/null || cargo install taplo-cli --locked
            cargo deny --version 2>/dev/null | grep -q '0.19.0' || cargo install cargo-deny --version 0.19.0 --locked
        '''
    }
    unix "python3 jenkins/ci_setup.py --target ${target} --env-file .ci-env"
}
