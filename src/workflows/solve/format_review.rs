//! "Solve: format review" — after the solve+validation stage, reformat the
//! finished netlist to the operator's house style so they have far less manual
//! cleanup to do. It's a STYLE-only pass (Claude, in the ngspice container so it
//! can verify the result still runs): terse human comments instead of verbose
//! AI prose, no decorative/extra spacing, and interactive `plot` instead of
//! `wrdata`-to-file. Two real, passing netlists are shipped as style exemplars.

use std::time::Duration;

use crate::prelude::*;

use super::util;

pub struct SolveFormatReview;

#[async_trait]
impl Workflow for SolveFormatReview {
    fn name(&self) -> &'static str {
        "Solve: format review"
    }
    fn description(&self) -> &'static str {
        "Reformat the solved netlist to the house style (terse comments, no extra spacing, interactive plots)."
    }
    fn requires_browser(&self) -> bool {
        false
    }
    fn inputs(&self) -> Vec<InputSpec> {
        vec![InputSpec::optional("task_id", "Task id (blank = newest bundle)", "")]
    }

    async fn run(&self, ctx: &mut WorkflowCtx) -> Result<WorkflowOutcome> {
        let task_id = ctx.input("task_id").map(str::to_string);
        let (id, _bundle) = util::find_bundle(&ctx.settings, task_id.as_deref())?;
        let ws = ctx.settings.output_dir.join("solve").join(&id);
        let sol = ws.join("final").join("solution.cir");

        let before = std::fs::read_to_string(&sol).map_err(|e| {
            GolemError::Io(format!("read netlist {}: {e} — run solve first", sol.display()))
        })?;
        if before.trim().is_empty() {
            return Err(ctx
                .stop_and_warn(format!("netlist {} is empty — run solve first", sol.display()))
                .await);
        }
        let before_lines = before.lines().count();

        // Ship the style exemplars next to the netlist for Claude to read.
        for (name, body) in [("format_ref_1.cir", REFERENCE_1), ("format_ref_2.cir", REFERENCE_2)] {
            let p = ws.join(name);
            std::fs::write(&p, body)
                .map_err(|e| GolemError::Io(format!("write {}: {e}", p.display())))?;
        }

        // --- provision an ngspice container so Claude can verify the reformat ---
        // (solve removes its container on success, so start a fresh one.)
        ctx.step("start ngspice container").await?;
        let container = util::container_name(&id);
        let image = util::image_tag(&ctx.settings);
        let ws_abs = util::absolute(&ws)?;
        let mount = format!("{}:/work", ws_abs.display());
        let _ = ctx
            .run("docker", &["rm", "-f", container.as_str()], None, Some(Duration::from_secs(30)))
            .await;
        let user = util::host_user();
        let mut run_args: Vec<&str> = vec!["run", "-d", "--name", container.as_str()];
        if let Some(u) = user.as_deref() {
            run_args.push("--user");
            run_args.push(u);
        }
        run_args.push("-v");
        run_args.push(mount.as_str());
        run_args.push(image.as_str());
        run_args.push("sleep");
        run_args.push("infinity");
        let started = ctx
            .run("docker", &run_args, None, Some(Duration::from_secs(120)))
            .await?;
        if !started.success() {
            return Err(ctx
                .stop_and_warn(format!("could not start container: {}", started.combined().trim()))
                .await);
        }

        // --- reformat with Claude ---
        ctx.step("reformat netlist (Claude)").await?;
        let prompt = format_prompt(&container);
        let timeout = Duration::from_secs(ctx.settings.claude_timeout_secs.max(60));
        let claude = util::claude_bin(&ctx.settings);
        let model = ctx.settings.solve_model.clone();
        let effort = ctx.settings.solve_effort.clone();
        let mut args: Vec<&str> = vec![
            "-p",
            prompt.as_str(),
            "--dangerously-skip-permissions",
            "--output-format",
            "stream-json",
            "--verbose",
        ];
        if !model.trim().is_empty() {
            args.push("--model");
            args.push(model.as_str());
        }
        if !effort.trim().is_empty() {
            args.push("--effort");
            args.push(effort.as_str());
        }
        let claude_res = ctx.run_claude(&claude, &args, Some(&ws), Some(timeout)).await;

        // --- cleanup: remove the container + exemplar files regardless of outcome ---
        let _ = ctx
            .run("docker", &["rm", "-f", container.as_str()], None, Some(Duration::from_secs(30)))
            .await;
        let _ = std::fs::remove_file(ws.join("format_ref_1.cir"));
        let _ = std::fs::remove_file(ws.join("format_ref_2.cir"));
        claude_res?;

        // --- sanity-check the result is still a runnable deck ---
        let after = std::fs::read_to_string(&sol).unwrap_or_default();
        if after.trim().is_empty() {
            return Err(ctx
                .stop_and_warn("the reformatted netlist is empty — the format pass failed; the original may be lost (check final/solution.cir).")
                .await);
        }
        let low = after.to_ascii_lowercase();
        if !low.contains(".end") {
            ctx.warn("reformatted netlist has no .end line — review it before running.");
        }
        if low.contains("wrdata") {
            ctx.warn("reformatted netlist still contains 'wrdata' — Claude may not have converted all plotting to interactive `plot`.");
        }
        let after_lines = after.lines().count();
        ctx.output(format!(
            "reformatted final/solution.cir to the house style ({before_lines} -> {after_lines} lines)"
        ));

        Ok(WorkflowOutcome::CompletedWith(json!({
            "task_id": id,
            "before_lines": before_lines,
            "after_lines": after_lines,
        })))
    }
}

fn format_prompt(container: &str) -> String {
    format!(
        "You are reformatting a FINISHED, WORKING ngspice netlist to match a house style, so a \
         human reviewer has minimal cleanup. Two reference netlists that already pass our format \
         AND functionality review are in this directory — read them to learn the style: \
         format_ref_1.cir and format_ref_2.cir. The netlist to reformat is final/solution.cir.\n\n\
         CRITICAL — this is a STYLE-ONLY rewrite. Do NOT change the circuit: keep every component, \
         node name, value, `.model` parameter, subcircuit, and every analysis (`.op`, `.ac`, \
         `.tran`, `.four`/`fourier`, `fft`, `meas`, `alter`, etc.) exactly as-is. Same circuit, \
         same results — only the presentation changes.\n\n\
         Apply the house style shown in the references:\n\
         1. COMMENTS: terse, human section headers only (e.g. `* Supplies`, `* common-emitter NPN \
            amplifier`, `* Models`). DELETE verbose AI-style comments, comments that restate the \
            obvious, and any prose explaining WHY something is done. A handful of short section \
            markers — not a narrative.\n\
         2. HORIZONTAL SPACING: separate tokens on a line with a SINGLE space — do NOT column-align \
            or pad with extra spaces. The references write `RA s2in nA 4.7k` and `V12 vcc 0 DC 12`, \
            never `Rwind vp  na    12` or `Vs   ns 0   DC 0`. Collapse every run of 2+ spaces \
            between tokens down to one (the `+` continuation lines and string literals are the only \
            places internal spacing is kept). Strip trailing whitespace.\n\
         3. BLANK LINES: KEEP them. The references put a single blank line between logical sections \
            (Supplies, each stage, Models, .control, etc.) — PRESERVE that structure; do NOT \
            collapse the file into one dense block. Only collapse runs of 2+ consecutive blank \
            lines down to a single blank line, and drop ASCII banners. Leave the section spacing \
            otherwise as-is.\n\
         4. PLOTTING: use ngspice INTERACTIVE plotting — `plot <exprs> title '...'` (and `fft` / \
            `fourier` as the references do), NOT `wrdata` to data files. If the deck writes data \
            files and/or shells out to gnuplot or generates SVG/hardcopy files, REPLACE that with \
            the equivalent interactive `plot ... title '...'` of the SAME quantities. Keep the \
            `.control` ... `.endc` block.\n\
         5. The result is ONE runnable `.cir` ending with `.endc` then `.end`.\n\n\
         VERIFY before finishing: run `docker exec {container} ngspice -b /work/final/solution.cir` \
         and confirm it still parses and the analyses/measurements run without errors. (Interactive \
         `plot` in batch mode just won't open a window — that's fine; ignore display-only warnings.) \
         If your reformat introduced any error, fix it WITHOUT changing the circuit.\n\n\
         Then OVERWRITE final/solution.cir with the reformatted netlist. Do not create other files."
    )
}

/// Real, passing reference netlist #1 (four-stage audio preamplifier) — the house
/// style for comments, spacing, and interactive `plot`-based verification.
const REFERENCE_1: &str = r#"* Four-Stage Audio Preamplifier
* structure: CE NPN gain stage -> active Baxandall tone control
* -> N-JFET variable-gain stage -> unity-gain opamp buffer

* Supplies
V12 vcc 0 DC 12
Vpos vp 0 DC 15
Vneg vn 0 DC -15
Vctrl vg3 0 DC -1.0

* Input source: 10mV peak, 1kHz, AC=1 for sweep
Vin src 0 DC 0 AC 1 SIN(0 10m 1k)

* common-emitter NPN amplifier
Cin1 src b1 1u
R1 vcc b1 47k
R2 b1 0 10k
RC vcc c1 5.6k
Q1 c1 b1 e1 QNPN
RE1 e1 e2 470
RE2 e2 0 1k
CE e2 0 100u
Cout1 c1 b2 1u

* Emitter-follower output buffer drives low-impedance tone stack
Rf1 vcc b2 100k
Rf2 b2 0 100k
Q2 vcc b2 s1o QNPN
REF s1o 0 4.7k
Ccpl s1o s2in 10u

* Active Baxandall tone control (low impedance)

* Bass network
RA s2in nA 4.7k
RC2 tout nB 4.7k
RB1 nA wb 50k
RB2 wb nB 50k
Cbass nA nB 82n
Rwb wb vinv 10

* Treble network
C1t s2in nC 8.2n
C2t tout nD 8.2n
RT1 nC wt 34k
RT2 wt nD 34k
Rwt wt vinv 10

X1 0 vinv vp vn tout OPAMP

* N-JFET common-source variable-gain
Cin3 tout g3 1u
RG vg3 g3 1meg
J1 d3 g3 s3 JNJF
RD vcc d3 2k
RS s3 0 470
CS s3 0 100u
Cout3 d3 s4in 1u

* Unity-gain opamp buffer driving 10k load
Rb4 s4in 0 100k
X2 s4in out vp vn out OPAMP
RL out 0 10k

* Models
.model QNPN NPN (IS=6.7e-15 BF=300 VAF=100 IKF=0.3 ISE=1e-13
+ NE=1.5 RB=10 RC=1 RE=0.5 CJE=4.5p CJC=3.5p TF=0.3n TR=10n)
.model JNJF NJF (VTO=-4.0 BETA=0.25m LAMBDA=2m RD=10 RS=10
+ CGS=4p CGD=4p)

* Opamp with ~10MHz GBW single pole
.subckt OPAMP ninv inv vcc vee out
Rin ninv inv 2meg
Gm 0 n1 ninv inv 1e-3
R1 n1 0 1e8
C1 n1 0 15.9p
Eb n2 0 n1 0 1
Ro n2 out 50
Dp out vcc DCLAMP
Dn vee out DCLAMP
.model DCLAMP D(IS=1e-14 N=1 CJO=0)
.ends

.control

* Tone-control resistor positions
* FLAT: RB1=RB2=50k RT1=RT2=34k
* BASS BOOST/TREBLE CUT: RB1=2k RB2=98k RT1=66.6k RT2=1.4k
* TREBLE BOOST/BASS CUT: RB1=98k RB2=2k RT1=1.4k RT2=66.6k
* JFET gate control voltage Vctrl: min=-3.0 mid=-1.0 max=+1.0

* DC op point
op
echo " "
echo "DC op point"
print v(c1) v(d3) v(tout) v(out)
echo " "

* Tone control AC sweeps

* run1 = flat
ac dec 50 20 100k
meas ac g_stage1 FIND vdb(s2in) AT=1000
meas ac g_total FIND vdb(out) AT=1000
meas ac g_200 FIND vdb(out) AT=200
meas ac g_20k FIND vdb(out) AT=20000

* run2 = bass boost/treble cut
alter RB1 = 2k
alter RB2 = 98k
alter RT1 = 66.6k
alter RT2 = 1.4k
ac dec 50 20 100k
meas ac bb_100 FIND vdb(out) AT=100
meas ac bb_1k FIND vdb(out) AT=1000
meas ac bb_10k FIND vdb(out) AT=10000

* run3 = treble boost/bass cut
alter RB1 = 98k
alter RB2 = 2k
alter RT1 = 1.4k
alter RT2 = 66.6k
ac dec 50 20 100k
meas ac tb_100 FIND vdb(out) AT=100
meas ac tb_1k FIND vdb(out) AT=1000
meas ac tb_10k FIND vdb(out) AT=10000

* restore
alter RB1 = 50k
alter RB2 = 50k
alter RT1 = 34k
alter RT2 = 34k

setplot ac1
let Flat = db(ac1.v(out))
let BassBoost_TrebleCut = db(ac2.v(out))
let TrebleBoost_BassCut = db(ac3.v(out))
plot Flat BassBoost_TrebleCut TrebleBoost_BassCut xlog xlimit 20 100k title 'Baxandall Tone Control Response'

* JFET ac sweeps
* min gain
alter Vctrl = -3.0
ac dec 50 20 100k
meas ac j_min FIND vdb(out) AT=1000
* mid gain
alter Vctrl = -1.0
ac dec 50 20 100k
meas ac j_mid FIND vdb(out) AT=1000
* max gain
alter Vctrl = 1.0
ac dec 50 20 100k
meas ac j_max FIND vdb(out) AT=1000
* back to midpoint
alter Vctrl = -1.0

setplot ac4
let Vctrl_min_m3 = db(ac4.v(out))
let Vctrl_mid_m1 = db(ac5.v(out))
let Vctrl_max_p1 = db(ac6.v(out))
plot Vctrl_min_m3 Vctrl_mid_m1 Vctrl_max_p1 xlog xlimit 20 100k title 'JFET Variable-Gain Stage'

* Transient
tran 5u 10m 0 5u
plot v(out) v(src) title 'Transient Response'

* Four/thd
linearize v(out) v(src)
fourier 1000 v(out)
* spectrum plot from the output transient waveform
fft v(out)
let spectrum_dBV = db(mag(v(out)))
plot spectrum_dBV xlog xlimit 100 50k title 'Output Spectrum'

.endc
.end
"#;

/// Real, passing reference netlist #2 (DCO audio synthesizer signal chain).
const REFERENCE_2: &str = r#"DCO-based Audio Synthesizer Analog Signal Chain

* Supply / ref rails
V_DC2 vsupply 0 DC 9
V_DC1 vref 0 DC 5

* Oscillator 0 : 550 Hz (period 1.818182 ms, half 0.909091 ms)
* V_OSC0 square -> divider R0_1/R0_2 -> coupling C0_1 -> re-bias R0_3 -> R0_4
V_OSC0 osc0 0 DC 0 AC 1 PULSE(0 5 0 1u 1u 0.909091m 1.818182m)
R0_1 osc0 div0 9k
R0_2 div0 0 1k
C0_1 div0 coup0 10u
R0_3 coup0 vref 100k
R0_4 coup0 summing 10k

* Oscillator 1 : 1100 Hz (period 0.909091 ms, half 0.454545 ms)
V_OSC1 osc1 0 DC 0 PULSE(0 5 0 1u 1u 0.454545m 0.909091m)
R1_1 osc1 div1 9k
R1_2 div1 0 1k
C1_1 div1 coup1 10u
R1_3 coup1 vref 100k
R1_4 coup1 summing 10k

* Oscillator 2 : 1650 Hz (period 0.606061 ms, half 0.303030 ms)
V_OSC2 osc2 0 DC 0 PULSE(0 5 0 1u 1u 0.303030m 0.606061m)
R2_1 osc2 div2 9k
R2_2 div2 0 1k
C2_1 div2 coup2 10u
R2_3 coup2 vref 100k
R2_4 coup2 summing 10k

* Inverting summing amplifier
RFB summing opamp_out 22k
XOP vref summing opamp_out vsupply 0 OPAMP

* Output coupling capacitor and load
C_OUT opamp_out vout 1u
RL vout 0 1000k

* Single-supply opamp
.subckt OPAMP inp inn out vcc vee
Rin inp inn 2Meg
Gm 0 n1 inp inn 100u
R1 n1 0 1G
C1 n1 0 0.31831p
Eb n2 0 n1 0 1
Rout n2 out 75
Dhi out vcc Dclamp
Dlo vee out Dclamp
.model Dclamp D(Is=1e-14 N=1)
.ends OPAMP
.control
op
echo "DC operating point"
print v(vref) v(summing) v(opamp_out) v(coup0) v(coup1) v(coup2)

* Transient analysis
* 500 ms run, 2 us step resolves all audio harmonics
tran 2u 500m
linearize
* raw oscillator outputs
plot v(osc0) v(osc1) v(osc2) xlimit 0 5m title 'DCO square-wave outputs (550/1100/1650 Hz)'
* AC-coupled re-biased inputs at the summing-resistor side
plot v(coup0) v(coup1) v(coup2) xlimit 0 5m title 'AC-coupled re-biased inputs'
* summing node (virtual ground) and amplified
plot v(summing) v(opamp_out) xlimit 0 5m title 'Summing node and amplifier output'
* final output-coupled signal into the load
plot v(vout) xlimit 0 5m title 'Output across load'
* THD check
let gainmag = 22k/10k
let comp_ac = -gainmag*((v(coup0)-5) + (v(coup1)-5) + (v(coup2)-5))
let out_ac = v(opamp_out)-5
let resid = out_ac - comp_ac
let thd_pct = 100 * sqrt(mean(resid*resid)) / sqrt(mean(out_ac*out_ac))
echo "THD of amplified output (percent)"
print thd_pct
* FFT of output
fft v(vout)
* Spectral peaks
let outmag = mag(v(vout))
meas sp f550 find outmag at=550
meas sp f1100 find outmag at=1100
meas sp f1650 find outmag at=1650
* output spectrum (audio band)
plot outmag xlimit 0 5k title 'Output spectrum w/ fundamental and harmonics'
* AC analysis, flat gain across band
ac dec 50 1 1Meg
echo "AC gain (dB) at band edges and center"
let gdb = vdb(opamp_out)
meas ac gain_20Hz find gdb at=20
meas ac gain_1kHz find gdb at=1k
meas ac gain_20kHz find gdb at=20k
* magnitude (dB) and phase across the audio band
plot vdb(opamp_out) title 'Amplifier gain magnitude (dB)'
plot vp(opamp_out) title 'Amplifier phase'
.endc
.end
"#;
