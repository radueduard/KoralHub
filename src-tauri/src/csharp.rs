//! C# projects: scenes written as C# scripts, run on the SDK's `koral-dotnet`.
//!
//! There is no build step Koral needs. `koral-dotnet` compiles the scripts in `src/` itself when it
//! starts, and again whenever one is saved (`--hot-reload`), so ▶ is simply "launch the runner on
//! `src/`". The project also carries a `.csproj`, which the runner ignores: it is what gives an
//! editor its completion and errors, and what the Build button type-checks with when a .NET SDK is
//! installed.
//!
//! The `.csproj` must find `Koral.dll` in the SDK, whose location is a per-machine fact. So, like the
//! C++ presets, it is kept out of the committed files: the Hub writes a gitignored
//! `koral.local.props` naming this machine's SDK, and the `.csproj` imports it.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::builder::{self, Console};
use crate::framework;
use crate::project;

/// The capability an SDK reports when it was built with its C# bindings (`-DKORAL_BUILD_DOTNET=ON`).
pub const CAPABILITY: &str = "csharp";

/// The file naming this machine's SDK for the `.csproj`. Written by the Hub; never committed.
pub const LOCAL_PROPS: &str = "koral.local.props";

/// Scaffold a new C# project's own files: its first scene, the `.csproj`, and what git ignores.
pub fn write_sources(root: &Path, name: &str) -> Result<(), String> {
    std::fs::write(root.join("src").join(format!("{name}.cs")), SCENE.replace("{NAME}", name))
        .map_err(|e| e.to_string())?;
    std::fs::write(root.join(format!("{name}.csproj")), CSPROJ).map_err(|e| e.to_string())?;
    std::fs::write(root.join(".gitignore"), GITIGNORE).map_err(|e| e.to_string())?;
    Ok(())
}

/// Point the `.csproj` at this machine's SDK. Best-effort at creation (the SDK may not be installed
/// yet); done again before every build, run and IDE open, so a moved SDK is followed.
pub fn write_local_props(root: &Path, sdk_tree: &Path) -> Result<(), String> {
    let props = format!(
        "<!-- Written by Koral Hub: where this machine's Koral SDK is. Not committed; regenerated as needed. -->\n\
         <Project>\n  <PropertyGroup>\n    <KoralSdk>{}</KoralSdk>\n  </PropertyGroup>\n</Project>\n",
        xml_escape(&sdk_tree.to_string_lossy())
    );
    std::fs::write(root.join(LOCAL_PROPS), props).map_err(|e| e.to_string())
}

/// VS Code, for a C# project: the C# extension pointed at the `dotnet` the Hub found (it looks on
/// the `PATH`, which a `~/.dotnet` install is not on — and without an SDK it cannot load the project,
/// so there is no completion), a launch configuration that runs the scenes under the C# debugger, and
/// the extension recommended. All per machine, and so gitignored (`.vscode/`), like the C++ ones.
pub fn write_vscode(root: &Path, sdk_tree: &Path) -> Result<(), String> {
    use serde_json::json;

    let dir = root.join(".vscode");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let dotnet = dotnet();
    let dotnet_root = dotnet.as_ref().and_then(|d| d.parent()).map(|d| d.to_string_lossy().into_owned());

    let mut settings = json!({
        // koral.local.props and bin/obj are the Hub's and the build's, not the project's.
        "files.exclude": { "**/bin": true, "**/obj": true },
    });
    if let Some(root) = &dotnet_root {
        settings["dotnet.dotnetPath"] = json!(root);
    }
    write_json(&dir.join("settings.json"), &settings)?;

    write_json(&dir.join("extensions.json"), &json!({ "recommendations": ["ms-dotnettools.csdevkit"] }))?;

    // The runner's apphost, started directly with DOTNET_ROOT set: that finds the runtime wherever it
    // is installed, and the C# debugger can launch an apphost like any program. The scripts it compiles
    // carry their PDBs and sources, so breakpoints in src/ bind. DOTNET_MODIFIABLE_ASSEMBLIES lets a saved
    // edit be applied to the running code in place — set here, because the runner cannot restart itself
    // with it under a debugger (the debugger would be left behind).
    let apphost = runner(sdk_tree)
        .map(|dll| dll.with_extension(if cfg!(windows) { "exe" } else { "" }))
        .map(|p| p.to_string_lossy().trim_end_matches('.').to_string())
        .unwrap_or_default();
    let mut args = vec!["${workspaceFolder}/src".to_string()];
    args.extend(builder::runtime_args(Path::new("")).into_iter().skip(1));
    args.push("--hot-reload".into());
    let mut env = json!({ "DOTNET_MODIFIABLE_ASSEMBLIES": "debug" });
    if let Some(root) = &dotnet_root {
        env["DOTNET_ROOT"] = json!(root);
    }

    // One configuration: F5 debugs it, Ctrl+F5 runs it without the debugger.
    let launch = json!({
        "version": "0.2.0",
        "configurations": [{
            "name": "Koral: Run scenes",
            "type": "coreclr",
            "request": "launch",
            "program": apphost,
            "args": args,
            "cwd": "${workspaceFolder}",
            "env": env,
            "console": "integratedTerminal",
            "stopAtEntry": false,
            "requireExactSource": false,
            "justMyCode": true,
        }],
    });
    write_json(&dir.join("launch.json"), &launch)?;

    // The same, as a task: Terminal → Run Task, for running with no debugger machinery at all.
    let tasks = json!({
        "version": "2.0.0",
        "tasks": [{
            "label": "Koral: Run scenes",
            "type": "process",
            "command": apphost,
            "args": args,
            "options": { "cwd": "${workspaceFolder}", "env": env },
            "problemMatcher": [],
            "presentation": { "reveal": "always", "panel": "dedicated" },
        }],
    });
    write_json(&dir.join("tasks.json"), &tasks)
}

fn write_json(path: &Path, value: &serde_json::Value) -> Result<(), String> {
    let text = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| format!("failed to write {}: {e}", path.display()))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The `dotnet` host: from `DOTNET_ROOT`, the `PATH`, or where the installers put it.
pub fn dotnet() -> Option<PathBuf> {
    let exe = if cfg!(windows) { "dotnet.exe" } else { "dotnet" };
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(root) = std::env::var_os("DOTNET_ROOT") {
        candidates.push(PathBuf::from(root).join(exe));
    }
    if let Some(found) = crate::ide::which("dotnet") {
        candidates.push(PathBuf::from(found));
    }
    if let Some(home) = dirs_home() {
        candidates.push(home.join(".dotnet").join(exe));
    }
    if cfg!(windows) {
        if let Some(pf) = std::env::var_os("ProgramFiles") {
            candidates.push(PathBuf::from(pf).join("dotnet").join(exe));
        }
    } else if cfg!(target_os = "macos") {
        candidates.push(PathBuf::from("/usr/local/share/dotnet/dotnet"));
        candidates.push(PathBuf::from("/opt/homebrew/bin/dotnet"));
    } else {
        candidates.push(PathBuf::from("/usr/share/dotnet/dotnet"));
        candidates.push(PathBuf::from("/usr/lib/dotnet/dotnet"));
    }
    // Resolved: a package manager's /usr/bin/dotnet is a link, and DOTNET_ROOT must be the folder the
    // runtimes are really in, not the link's.
    candidates.into_iter().find(|p| p.is_file()).map(|p| std::fs::canonicalize(&p).unwrap_or(p))
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from)
}

/// Whether `dotnet` has a runtime new enough for the runner (it rolls forward to later majors).
fn has_runtime(dotnet: &Path) -> bool {
    listed(dotnet, "--list-runtimes").iter().any(|line| {
        line.strip_prefix("Microsoft.NETCore.App ")
            .and_then(|v| v.split('.').next())
            .and_then(|major| major.parse::<u32>().ok())
            .is_some_and(|major| major >= 10)
    })
}

/// Whether `dotnet` can build (an SDK, not only a runtime).
fn has_sdk(dotnet: &Path) -> bool {
    !listed(dotnet, "--list-sdks").is_empty()
}

fn listed(dotnet: &Path, flag: &str) -> Vec<String> {
    Command::new(dotnet)
        .arg(flag)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().map(str::to_owned).filter(|l| !l.trim().is_empty()).collect())
        .unwrap_or_default()
}

/// The runner in an SDK tree: `lib/koral-dotnet/koral-dotnet.dll`.
fn runner(sdk_tree: &Path) -> Option<PathBuf> {
    ["lib", "lib64"]
        .iter()
        .map(|lib| sdk_tree.join(lib).join("koral-dotnet").join("koral-dotnet.dll"))
        .find(|p| p.is_file())
}

/// The SDK tree this project runs on, checked for C#.
fn sdk_tree(root: &Path, version: &str, profile: &str) -> Result<PathBuf, String> {
    let (sdk_root, _) = framework::resolve(version)?;
    let tree = framework::tree_for_profile(&sdk_root, profile);
    if !framework::Capabilities::read(&tree).has(CAPABILITY) {
        return Err(format!(
            "Koral {version} was built without its C# bindings, so it cannot run C# scenes. Use a \
             release that has them, or build Koral with -DKORAL_BUILD_DOTNET=ON."
        ));
    }
    write_local_props(root, &tree)?;
    write_vscode(root, &tree)?;
    Ok(tree)
}

/// Keep the `.csproj` pointed at this machine's SDK, for an IDE about to open the project.
pub fn prepare(root: &Path) -> Result<(), String> {
    let cfg = project::load(root)?;
    sdk_tree(root, &cfg.framework_version, &project::profile(root)).map(|_| ())
}

/// Build: type-check the scripts with `dotnet build`, when there is a .NET SDK to do it with.
pub fn build(console: &Console, profile: &str) -> Result<PathBuf, String> {
    let root = console.root().to_path_buf();
    let cfg = project::load(&root)?;
    console.build(&format!("Resolving koral {}…\n", cfg.framework_version));
    let tree = sdk_tree(&root, &cfg.framework_version, profile)?;

    let csproj = root.join(format!("{}.csproj", cfg.name));
    match dotnet() {
        Some(dotnet) if has_sdk(&dotnet) && csproj.is_file() => {
            console.build(&format!("$ dotnet build {}\n", csproj.display()));
            let mut cmd = builder::external_command(dotnet.into_os_string());
            cmd.arg("build").arg(&csproj).arg("--nologo").arg("-v").arg("quiet").current_dir(&root);
            builder::run_step(console, &mut cmd)?;
        }
        _ => console.build(
            "Nothing to build: koral-dotnet compiles the scripts itself when it runs. (With a .NET SDK \
             installed, Build type-checks them here first.)\n",
        ),
    }
    Ok(tree)
}

/// Run: launch `koral-dotnet` on `src/`, reloading the scripts as they are saved.
pub fn run(console: &Console, profile: &str) -> Result<(), String> {
    let tree = build(console, profile)?;
    let root = console.root().to_path_buf();

    let runner = runner(&tree).ok_or_else(|| {
        format!("koral-dotnet is not in the SDK at {} — was it installed with -DKORAL_BUILD_DOTNET=ON?", tree.display())
    })?;
    let dotnet = dotnet().ok_or(
        "C# scenes run on .NET 10, which is not installed here. Install the .NET 10 runtime \
         (https://dotnet.microsoft.com/download) and run again.",
    )?;
    if !has_runtime(&dotnet) {
        return Err(format!(
            "{} has no .NET 10 runtime. Install it (https://dotnet.microsoft.com/download) and run again.",
            dotnet.display()
        ));
    }

    // The runner's arguments are the C++ runtime's (koral.json is found from src/ up), after the
    // runner itself: `dotnet koral-dotnet.dll <scripts> [--platform …] --hot-reload`.
    let mut args = vec![runner.to_string_lossy().into_owned()];
    args.extend(builder::runtime_args(&root.join("src")));
    args.push("--hot-reload".into());

    console.run(&format!("$ {} {}\n", dotnet.display(), args.join(" ")));
    builder::launch(console, &dotnet, &args)
}

const SCENE: &str = r#"// {NAME}: a Koral scene in C#. ▶ in the Hub runs it; save this file while it runs and it reloads,
// keeping its [Keep] state. The API is Koral's C++ API — see docs/csharp.md in the SDK.

/// Clears the screen, so what the passes after it draw is all there is.
public sealed class Clear(Vector4 color) : RenderPass("Clear")
{
    private Image? _screen;

    public override void Setup(PassBuilder builder) => builder.Write(FrameGraph.Screen, Image.Usage.eTransferDst);
    public override void Initialize(PassResources resources) => _screen = resources.ImageNamed(FrameGraph.Screen);
    public override void Record(CommandBuffer commandBuffer) => commandBuffer.ClearColorImage(_screen!, color);
}

public sealed class {NAME} : Scene
{
    [Keep] private float _angle;

    protected override void Initialize()
    {
        Input.BindAction("Quit", Key.eEsc, GamepadButton.eBack);
        Graph.Add(new Clear(new Vector4(0.05f, 0.06f, 0.09f, 1f)));
        Graph.Add(new DebugDrawPass(SceneDebug, Camera));
    }

    private Matrix4x4 Camera()
    {
        var extent = Window.Extent;
        var view = Matrix4x4.CreateLookAt(new Vector3(0, 3, 6), Vector3.Zero, Vector3.UnitY);
        var projection = Matrix4x4.CreatePerspectiveFieldOfView(MathF.PI / 3, (float)extent.X / Math.Max(extent.Y, 1u), 0.1f, 100f);
        projection.M22 *= -1;   // Vulkan's Y points down
        return view * projection;
    }

    protected override void Update()
    {
        if (Input.IsActionPressed("Quit")) Navigator.Quit();

        _angle += Time.FrameTime;
        Debug.Grid(Vector3.Zero, 10f, 10, new DebugStyle { Color = new Vector4(0.25f, 0.25f, 0.3f, 1f) });
        Debug.Box(Matrix4x4.CreateRotationY(_angle) * Matrix4x4.CreateTranslation(0, 0.5f, 0),
                  new DebugStyle { Color = new Vector4(0.3f, 0.8f, 1f, 1f) });
    }
}
"#;

const CSPROJ: &str = r#"<Project Sdk="Microsoft.NET.Sdk">
  <!-- For editors, and the Hub's Build: koral-dotnet compiles src/ itself and does not read this file.
       koral.local.props (written by the Hub, not committed) says where this machine's Koral SDK is. -->
  <Import Project="koral.local.props" Condition="Exists('koral.local.props')" />

  <PropertyGroup>
    <TargetFramework>net10.0</TargetFramework>
    <OutputType>Library</OutputType>
    <Nullable>enable</Nullable>
    <ImplicitUsings>enable</ImplicitUsings>
    <EnableDefaultCompileItems>false</EnableDefaultCompileItems>
  </PropertyGroup>

  <ItemGroup>
    <Compile Include="src/**/*.cs" />
    <Reference Include="Koral">
      <HintPath>$(KoralSdk)/lib/koral-dotnet/Koral.dll</HintPath>
      <Private>false</Private>
    </Reference>
    <!-- koral-ui's C# binding: interfaces (widgets, the canvas). koral-dotnet ships it beside Koral.dll. -->
    <Reference Include="Koral.UI">
      <HintPath>$(KoralSdk)/lib/koral-dotnet/Koral.UI.dll</HintPath>
      <Private>false</Private>
    </Reference>
  </ItemGroup>

  <!-- What koral-dotnet imports into every script, so the editor sees the same code. -->
  <ItemGroup>
    <Using Include="System.Numerics" />
    <Using Include="Koral" />
    <Using Include="Koral.Buffer" Alias="Buffer" />
  </ItemGroup>
</Project>
"#;

const GITIGNORE: &str = r#"# Build output
bin/
obj/

# Per machine: where the Koral SDK is (written by the Hub)
koral.local.props

# Per user: ImGui layouts, IDE state
imgui*.ini
.vs/
.idea/
.vscode/
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_project_has_a_scene_a_csproj_and_ignores_the_machine() {
        let dir = std::env::temp_dir().join(format!("koral-csharp-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        write_sources(&dir, "Orbit").unwrap();
        let scene = std::fs::read_to_string(dir.join("src/Orbit.cs")).unwrap();
        assert!(scene.contains("public sealed class Orbit : Scene"));
        let csproj = std::fs::read_to_string(dir.join("Orbit.csproj")).unwrap();
        assert!(csproj.contains("$(KoralSdk)/lib/koral-dotnet/Koral.dll"));
        assert!(csproj.contains("$(KoralSdk)/lib/koral-dotnet/Koral.UI.dll"));
        assert!(std::fs::read_to_string(dir.join(".gitignore")).unwrap().contains(LOCAL_PROPS));

        write_local_props(&dir, Path::new("/opt/koral & co")).unwrap();
        let props = std::fs::read_to_string(dir.join(LOCAL_PROPS)).unwrap();
        assert!(props.contains("<KoralSdk>/opt/koral &amp; co</KoralSdk>"), "{props}");

        write_vscode(&dir, Path::new("/opt/koral")).unwrap();
        let launch: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join(".vscode/launch.json")).unwrap()).unwrap();
        assert_eq!(launch["configurations"][0]["type"], "coreclr");
        assert_eq!(launch["configurations"][0]["env"]["DOTNET_MODIFIABLE_ASSEMBLIES"], "debug");
        assert!(dir.join(".vscode/tasks.json").is_file());
        assert_eq!(launch["configurations"][0]["args"][0], "${workspaceFolder}/src");
        assert!(dir.join(".vscode/extensions.json").is_file());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}



