//! Kotlin projects: scenes written in Kotlin, with Jetpack Compose interfaces, built by Gradle.
//!
//! A Kotlin project is an ordinary Gradle project. It depends on `koral:koral-ui` from the Maven
//! repository the SDK carries (`share/Koral/maven`), and its `main` is `App.launch(args) { … }`, which
//! reads koral.json the runtime's way. ▶ runs Gradle's `hotRun`: edits apply to the running program
//! as they are saved — any edit on the JetBrains Runtime, method bodies on another JDK.
//!
//! Where the SDK and the JDK are is a per-machine fact, kept out of the committed files like the C++
//! presets and C#'s `koral.local.props`: the Hub writes a gitignored `gradle.properties` naming them,
//! which Gradle, and any IDE opening the project with Gradle, reads.

use std::path::{Path, PathBuf};

use crate::builder::{self, Console};
use crate::framework;
use crate::project;

/// The capability an SDK reports when it was built with its Kotlin bindings (`-DKORAL_BUILD_KOTLIN=ON`).
pub const CAPABILITY: &str = "kotlin";

/// The file naming this machine's SDK and JDK for Gradle. Written by the Hub; never committed.
pub const MACHINE_PROPERTIES: &str = "gradle.properties";

/// The JDK version the bindings need: java.lang.foreign, and the Kotlin they are compiled for.
const JAVA: u32 = 25;

/// Scaffold a new Kotlin project's own files: its first scene, the Gradle build, and what git ignores.
pub fn write_sources(root: &Path, name: &str) -> Result<(), String> {
    let kotlin = root.join("src").join("main").join("kotlin");
    std::fs::create_dir_all(&kotlin).map_err(|e| e.to_string())?;
    std::fs::write(kotlin.join(format!("{name}.kt")), SCENE.replace("{NAME}", name)).map_err(|e| e.to_string())?;
    std::fs::write(root.join("settings.gradle.kts"), SETTINGS.replace("{NAME}", name)).map_err(|e| e.to_string())?;
    std::fs::write(root.join("build.gradle.kts"), BUILD.replace("{NAME}", name)).map_err(|e| e.to_string())?;
    std::fs::write(root.join(".gitignore"), GITIGNORE).map_err(|e| e.to_string())?;
    Ok(())
}

/// What depends on this machine: the Gradle wrapper (copied from the SDK when the project has none),
/// `gradle.properties` naming the SDK and the JDKs, and VS Code's files. Best-effort at creation;
/// done again before every build, run and IDE open, so a moved SDK or a new JDK is followed.
pub fn write_machine_files(root: &Path, sdk_tree: &Path) -> Result<(), String> {
    copy_wrapper(root, sdk_tree)?;
    write_properties(root, sdk_tree, jdk().as_deref(), jbr().as_deref())?;
    write_vscode(root)
}

/// The SDK's Gradle wrapper, into a project that has none: it then builds with no Gradle installed.
fn copy_wrapper(root: &Path, sdk_tree: &Path) -> Result<(), String> {
    if root.join("gradle").join("wrapper").join("gradle-wrapper.jar").is_file() {
        return Ok(());
    }
    let from = sdk_tree.join("share").join("Koral").join("kotlin").join("wrapper");
    if !from.join("gradlew").is_file() {
        return Err(format!("the SDK at {} has no Gradle wrapper (share/Koral/kotlin/wrapper)", sdk_tree.display()));
    }
    for file in ["gradlew", "gradlew.bat", "gradle/wrapper/gradle-wrapper.jar", "gradle/wrapper/gradle-wrapper.properties"] {
        let to = root.join(file);
        if let Some(dir) = to.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::copy(from.join(file), &to).map_err(|e| format!("failed to copy {file}: {e}"))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let gradlew = root.join("gradlew");
        let _ = std::fs::set_permissions(&gradlew, std::fs::Permissions::from_mode(0o755));
    }
    Ok(())
}

fn write_properties(root: &Path, sdk_tree: &Path, jdk: Option<&Path>, jbr: Option<&Path>) -> Result<(), String> {
    // A properties file's backslash is an escape: Windows paths are written with forward slashes.
    let path = |p: &Path| p.to_string_lossy().replace('\\', "/");
    let mut text = String::from(
        "# Written by Koral Hub: where this machine's Koral SDK and JDKs are. Not committed; regenerated as needed.\n",
    );
    text += &format!("koral.sdk={}\n", path(sdk_tree));
    if let Some(jdk) = jdk {
        text += &format!("org.gradle.java.home={}\n", path(jdk));
    }
    if let Some(jbr) = jbr {
        text += &format!("koral.jbr={}\n", path(jbr));
    }
    std::fs::write(root.join(MACHINE_PROPERTIES), text).map_err(|e| e.to_string())
}

/// VS Code: the Kotlin and Gradle extensions recommended, and ▶'s run as a task. Per machine, and so
/// gitignored (`.vscode/`). IntelliJ IDEA needs nothing: it opens the Gradle build itself.
fn write_vscode(root: &Path) -> Result<(), String> {
    use serde_json::json;

    let dir = root.join(".vscode");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    write_json(&dir.join("extensions.json"), &json!({ "recommendations": ["fwcd.kotlin", "vscjava.vscode-gradle"] }))?;
    write_json(&dir.join("settings.json"), &json!({ "files.exclude": { "**/build": true, "**/.gradle": true, "**/.kotlin": true } }))?;
    let gradlew = if cfg!(windows) { "${workspaceFolder}\\gradlew.bat" } else { "${workspaceFolder}/gradlew" };
    write_json(&dir.join("tasks.json"), &json!({
        "version": "2.0.0",
        "tasks": [{
            "label": "Koral: Run scenes",
            "type": "process",
            "command": gradlew,
            "args": ["hotRun", "--console=plain"],
            "options": { "cwd": "${workspaceFolder}" },
            "problemMatcher": [],
            "presentation": { "reveal": "always", "panel": "dedicated" },
        }],
    }))
}

fn write_json(path: &Path, value: &serde_json::Value) -> Result<(), String> {
    let text = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| format!("failed to write {}: {e}", path.display()))
}

// ---- JDKs ------------------------------------------------------------------------------------------

/// A JDK home's major version and whether it is the JetBrains Runtime, from its `release` file.
fn describe(home: &Path) -> Option<(u32, bool)> {
    let release = std::fs::read_to_string(home.join("release")).ok()?;
    let value = |key: &str| {
        release.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix('=').map(|v| v.trim().trim_matches('"').to_string()))
    };
    let major = value("JAVA_VERSION")?.split(['.', '-', '+']).next()?.parse().ok()?;
    let jetbrains = value("IMPLEMENTOR").is_some_and(|i| i.contains("JetBrains"));
    Some((major, jetbrains))
}

/// Every JDK home this machine has where installers and people put them.
fn candidates() -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    for var in ["KORAL_JBR", "JAVA_HOME"] {
        if let Some(home) = std::env::var_os(var) {
            found.push(PathBuf::from(home));
        }
    }
    if let Some(java) = crate::ide::which("java") {
        // bin/java's home, through any links (a distribution's /usr/bin/java).
        let java = std::fs::canonicalize(&java).unwrap_or_else(|_| PathBuf::from(&java));
        if let Some(home) = java.parent().and_then(Path::parent) {
            found.push(home.to_path_buf());
        }
    }
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from);
    let mut parents: Vec<PathBuf> = Vec::new();
    if let Some(home) = &home {
        parents.push(home.join(".local").join("jdk"));
        parents.push(home.join(".jdks")); // IntelliJ's downloads
        parents.push(home.join(".sdkman").join("candidates").join("java"));
    }
    if cfg!(windows) {
        for pf in ["ProgramFiles", "ProgramW6432"] {
            if let Some(pf) = std::env::var_os(pf) {
                for vendor in ["Java", "Eclipse Adoptium", "Microsoft", "Zulu", "JetBrains"] {
                    parents.push(PathBuf::from(&pf).join(vendor));
                }
            }
        }
    } else if cfg!(target_os = "macos") {
        parents.push(PathBuf::from("/Library/Java/JavaVirtualMachines"));
        if let Some(home) = &home {
            parents.push(home.join("Library").join("Java").join("JavaVirtualMachines"));
        }
    } else {
        parents.push(PathBuf::from("/usr/lib/jvm"));
    }
    for parent in parents {
        let Ok(entries) = std::fs::read_dir(&parent) else { continue };
        for entry in entries.flatten() {
            let dir = entry.path();
            // A macOS bundle keeps its home under Contents/Home.
            let mac = dir.join("Contents").join("Home");
            found.push(if mac.is_dir() { mac } else { dir });
        }
    }
    found
}

/// The newest JDK of version 25 or later, preferring one that is not the JetBrains Runtime (Gradle runs on it).
pub fn jdk() -> Option<PathBuf> {
    let mut all: Vec<(u32, bool, PathBuf)> =
        candidates().into_iter().filter_map(|h| describe(&h).map(|(v, jb)| (v, jb, h))).filter(|(v, _, _)| *v >= JAVA).collect();
    all.sort_by_key(|(v, jb, _)| (*jb, std::cmp::Reverse(*v)));
    all.into_iter().next().map(|(_, _, h)| h)
}

/// A JetBrains Runtime of version 25 or later: what applies any edit while a program runs.
pub fn jbr() -> Option<PathBuf> {
    candidates().into_iter().find(|h| describe(h).is_some_and(|(v, jb)| jb && v >= JAVA))
}

// ---- building and running --------------------------------------------------------------------------

/// The SDK tree this project builds against, checked for Kotlin, with the machine's files written.
fn sdk_tree(root: &Path, version: &str, profile: &str) -> Result<PathBuf, String> {
    let (sdk_root, _) = framework::resolve(version)?;
    let tree = framework::tree_for_profile(&sdk_root, profile);
    if !framework::Capabilities::read(&tree).has(CAPABILITY) {
        return Err(format!(
            "Koral {version} was built without its Kotlin bindings, so it cannot run Kotlin scenes. Use a \
             release that has them, or build Koral with -DKORAL_BUILD_KOTLIN=ON."
        ));
    }
    write_machine_files(root, &tree)?;
    Ok(tree)
}

/// Keep `gradle.properties` pointed at this machine's SDK and JDK, for an IDE about to open the project.
pub fn prepare(root: &Path) -> Result<(), String> {
    let cfg = project::load(root)?;
    sdk_tree(root, &cfg.framework_version, &project::profile(root)).map(|_| ())
}

fn gradlew(root: &Path) -> PathBuf {
    root.join(if cfg!(windows) { "gradlew.bat" } else { "gradlew" })
}

/// What Gradle runs with: a JDK 25 to start on (`JAVA_HOME`), and where the SDK is for the program.
fn environment(tree: &Path) -> Result<Vec<(String, String)>, String> {
    let jdk = jdk().ok_or(
        "Kotlin scenes need JDK 25 or later, which is not installed here. Install one (https://adoptium.net) \
         and run again.",
    )?;
    Ok(vec![
        ("JAVA_HOME".into(), jdk.to_string_lossy().into_owned()),
        ("KORAL_SDK".into(), tree.to_string_lossy().into_owned()),
    ])
}

/// Build: compile the sources with Gradle (its first run downloads Gradle and the libraries).
pub fn build(console: &Console, profile: &str) -> Result<PathBuf, String> {
    let root = console.root().to_path_buf();
    let cfg = project::load(&root)?;
    console.build(&format!("Resolving koral {}…\n", cfg.framework_version));
    let tree = sdk_tree(&root, &cfg.framework_version, profile)?;

    let gradlew = gradlew(&root);
    console.build(&format!("$ {} classes\n", gradlew.display()));
    let mut cmd = builder::external_command(&gradlew);
    cmd.arg("classes").arg("--console=plain").arg("-q").current_dir(&root);
    for (key, value) in environment(&tree)? {
        cmd.env(key, value);
    }
    builder::run_step(console, &mut cmd)?;
    Ok(tree)
}

/// Run: Gradle's `hotRun`, which applies edits as the sources are saved.
pub fn run(console: &Console, profile: &str) -> Result<(), String> {
    let tree = build(console, profile)?;
    let root = console.root().to_path_buf();
    if jbr().is_none() {
        console.run(
            "Edits to what methods do apply while it runs. For any edit (an added composable, a new \
             function), install the JetBrains Runtime 25: https://github.com/JetBrains/JetBrainsRuntime/releases\n",
        );
    }

    // The runtime's own flags (--platform …), after the task: koral.json is found from the project.
    let flags: Vec<String> = builder::runtime_args(Path::new("")).into_iter().skip(1).collect();
    // `-p`: launch does not set the working directory, and Gradle would otherwise look for a build there.
    let mut args = vec!["-p".to_string(), root.to_string_lossy().into_owned(), "hotRun".into(), "--console=plain".into(), "-q".into()];
    if !flags.is_empty() {
        args.push(format!("--args={}", flags.join(" ")));
    }
    let gradlew = gradlew(&root);
    console.run(&format!("$ {} {}\n", gradlew.display(), args.join(" ")));
    builder::launch_env(console, &gradlew, &args, &environment(&tree)?)
}

const SCENE: &str = r#"// {NAME}: a Koral scene in Kotlin. ▶ in the Hub runs it; save this file while it runs and the edit
// is applied to it, keeping its state. The API is Koral's C++ API — see docs/kotlin.md in the SDK.

import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import koral.*
import koral.compose.*

/** Clears the screen, so what the passes after it draw is all there is. */
class Clear(private val color: Vec4) : RenderPass("Clear") {
    private lateinit var screen: koral.Image

    override fun setup(builder: PassBuilder) { builder.write(FrameGraph.Screen, ImageUsage.eTransferDst) }
    override fun initialize(resources: PassResources) { screen = resources.imageNamed(FrameGraph.Screen) }
    override fun record(commands: CommandBuffer) { commands.clearColorImage(screen, color) }
}

class {NAME} : Scene() {
    private var angle = 0f

    override fun initialize() {
        input.bindAction("Quit", InputSource(Key.eEsc), InputSource(GamepadButton.eBack))
        graph.add(Clear(Vec4(0.05f, 0.06f, 0.09f, 1f)))
        graph.add(DebugDrawPass(debug, ::camera))
        setContent { Panel() }
    }

    private fun camera(): Mat4 {
        val extent = window.extent
        val view = Mat4.lookAt(Vec3(0f, 3f, 6f), Vec3.Zero)
        val projection = Mat4.perspective(Math.PI.toFloat() / 3f, extent.x.toFloat() / maxOf(extent.y, 1), 0.1f, 100f)
            .toArray().also { it[5] = -it[5] }   // Vulkan's Y points down
        return Mat4(projection) * view
    }

    override fun update() {
        if (input.isActionPressed("Quit")) Navigator.quit()

        angle += time.frameTime
        debug.grid(Vec3.Zero, 10f, 10, DebugStyle(Vec4(0.25f, 0.25f, 0.3f, 1f)))
        debug.box(Mat4.translation(Vec3(0f, 0.5f, 0f)) * Mat4.rotation(angle, Vec3.Up), DebugStyle(Vec4(0.3f, 0.8f, 1f, 1f)))
    }
}

/** An interface over the scene, in Jetpack Compose. */
@Composable
fun Panel() {
    var clicks by remember { mutableStateOf(0) }
    Column(Modifier.padding(16.dp).background(Color(0xCC1E2028), RoundedCornerShape(10.dp)).padding(14.dp),
           verticalArrangement = Arrangement.spacedBy(8.dp)) {
        Text("{NAME}", fontSize = 20.sp)
        Text("Esc quits. Save the file while it runs to see an edit.", color = LocalTheme.current.textMuted)
        Button(onClick = { clicks++ }) { Text(if (clicks == 0) "Click me" else "Clicked $clicks times") }
    }
}

fun main(args: Array<String>) = App.launch(args) { register<{NAME}>() }
"#;

const SETTINGS: &str = r#"pluginManagement {
    repositories { gradlePluginPortal(); mavenCentral() }
}
rootProject.name = "{NAME}"
"#;

const BUILD: &str = r#"// A Koral project in Kotlin. Koral Hub writes gradle.properties (not committed): koral.sdk, where this
// machine's Koral SDK is, and the JDKs to run on. Without the Hub, set KORAL_SDK instead.
plugins {
    kotlin("jvm") version "2.4.20"
    kotlin("plugin.compose") version "2.4.20"
    application
}

val koralSdk: String = (findProperty("koral.sdk") as String?)?.takeIf { it.isNotBlank() } ?: System.getenv("KORAL_SDK")
    ?: error("Koral's SDK was not found: open the project in Koral Hub, or set KORAL_SDK")

repositories {
    maven(url = file("$koralSdk/share/Koral/maven"))   // koral and koral-ui, as the SDK carries them
    mavenCentral()
    google()
}

dependencies {
    implementation("koral:koral-ui:+")   // the SDK's own version: its repository has no other
}

kotlin { compilerOptions { jvmTarget.set(org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_25) } }
java { sourceCompatibility = JavaVersion.VERSION_25; targetCompatibility = JavaVersion.VERSION_25 }

application { mainClass.set("{NAME}Kt") }

tasks.withType<JavaExec>().configureEach {
    jvmArgs("--enable-native-access=ALL-UNNAMED")
    environment("KORAL_SDK", koralSdk)
    workingDir = projectDir   // where koral.json is found from
}

// ▶ in Koral Hub: the program, with edits applied as the sources are saved. Koral's jar is the JVM agent
// that applies them; on the JetBrains Runtime (koral.jbr) any edit applies, elsewhere what methods do.
tasks.register<JavaExec>("hotRun") {
    group = "application"
    description = "Runs the scenes, applying source edits while they run"
    dependsOn(tasks.classes)
    classpath = sourceSets.main.get().runtimeClasspath
    mainClass.set(application.mainClass)
    val koralJar = configurations.runtimeClasspath.map { files -> files.first { it.name.matches(Regex("koral-[0-9].*\\.jar")) } }
    jvmArgumentProviders.add(CommandLineArgumentProvider { listOf("-javaagent:" + koralJar.get()) })
    ((findProperty("koral.jbr") as String?)?.takeIf { it.isNotBlank() } ?: System.getenv("KORAL_JBR"))?.let {
        executable("$it/bin/java")
        jvmArgs("-XX:+AllowEnhancedClassRedefinition")
    }
    val wrapper = rootDir.resolve(if (System.getProperty("os.name").startsWith("Windows")) "gradlew.bat" else "gradlew")
    systemProperty("koral.hotReload", "true")
    systemProperty("koral.hotReload.sources", file("src/main/kotlin").absolutePath)
    systemProperty("koral.hotReload.compile", "\"$wrapper\" -p \"$rootDir\" -q --offline classes")
}
"#;

const GITIGNORE: &str = r#"# Build output
build/
.gradle/
.kotlin/

# Per machine: where the Koral SDK and the JDKs are (written by Koral Hub)
gradle.properties

# Per user: ImGui layouts, IDE state
imgui*.ini
.idea/
.vscode/
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_project_is_a_gradle_build_that_ignores_the_machine() {
        let dir = std::env::temp_dir().join(format!("koral-kotlin-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        write_sources(&dir, "Orbit").unwrap();
        let scene = std::fs::read_to_string(dir.join("src/main/kotlin/Orbit.kt")).unwrap();
        assert!(scene.contains("class Orbit : Scene()"));
        assert!(scene.contains("App.launch(args) { register<Orbit>() }"));
        let build = std::fs::read_to_string(dir.join("build.gradle.kts")).unwrap();
        assert!(build.contains("koral:koral-ui:+") && build.contains("mainClass.set(\"OrbitKt\")"));
        assert!(std::fs::read_to_string(dir.join("settings.gradle.kts")).unwrap().contains("rootProject.name = \"Orbit\""));
        assert!(std::fs::read_to_string(dir.join(".gitignore")).unwrap().contains(MACHINE_PROPERTIES));

        write_properties(&dir, Path::new("C:\\Koral SDK"), Some(Path::new("/opt/jdk")), None).unwrap();
        let props = std::fs::read_to_string(dir.join(MACHINE_PROPERTIES)).unwrap();
        assert!(props.contains("koral.sdk=C:/Koral SDK\n"), "{props}");
        assert!(props.contains("org.gradle.java.home=/opt/jdk\n") && !props.contains("koral.jbr"), "{props}");

        write_vscode(&dir).unwrap();
        let tasks: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join(".vscode/tasks.json")).unwrap()).unwrap();
        assert_eq!(tasks["tasks"][0]["args"][0], "hotRun");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_jdk_is_told_by_its_release_file() {
        let dir = std::env::temp_dir().join(format!("koral-kotlin-jdk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("release"), "IMPLEMENTOR=\"JetBrains s.r.o.\"\nJAVA_VERSION=\"25.0.4.1\"\n").unwrap();
        assert_eq!(describe(&dir), Some((25, true)));
        std::fs::write(dir.join("release"), "IMPLEMENTOR=\"Eclipse Adoptium\"\nJAVA_VERSION=\"21.0.2\"\n").unwrap();
        assert_eq!(describe(&dir), Some((21, false)));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

/// Makes a project at `KORAL_KOTLIN_PROJECT` against the SDK tree `KORAL_KOTLIN_SDK`, as the Hub would:
/// what a manual end-to-end check builds and runs. `cargo test --lib scaffold_for_a_manual_check -- --ignored`.
#[cfg(test)]
#[test]
#[ignore]
fn scaffold_for_a_manual_check() {
    let root = PathBuf::from(std::env::var("KORAL_KOTLIN_PROJECT").unwrap());
    let sdk = PathBuf::from(std::env::var("KORAL_KOTLIN_SDK").unwrap());
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("koral.json"), "{\n  \"name\": \"Orbit\",\n  \"language\": \"kotlin\"\n}\n").unwrap();
    write_sources(&root, "Orbit").unwrap();
    write_machine_files(&root, &sdk).unwrap();
    println!("JDK {:?} JBR {:?}", jdk(), jbr());
}
