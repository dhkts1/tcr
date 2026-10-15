import Foundation

/// The Peers words that change when the fleet holds a Codex account, kept in one
/// place a test can read.
///
/// No Mac lends, serves or carries a Codex request in this version: a Codex request
/// always leaves from this Mac. So once the fleet holds a Codex account, every
/// promise the Peers tab and Settings › Peers make about "your requests" or "your
/// traffic" names Claude, and one line, ``codexStaysOnThisMac``, says where Codex
/// requests go instead. A Claude-only fleet passes `holdsCodex: false` everywhere and
/// reads today's words, unchanged.
public enum PeersCodexCopy {
    /// The one line that says where Codex requests go: under the Share card, and in
    /// the Sharing head of Settings › Peers.
    public static let codexStaysOnThisMac =
        "Codex requests always leave from this Mac: no Mac lends, serves or carries them in "
        + "this version."

    /// The lease sheet's caption under its scope, which never offers a Codex account.
    public static let leaseCaption = "Codex accounts stay on this Mac."

    // MARK: The Peers tab's Share card

    public static func shareCardTitle(holdsCodex: Bool) -> String {
        "Share \(claude(holdsCodex))accounts with trusted Macs"
    }

    public static func shareStateOff(holdsCodex: Bool) -> String {
        "Off. Trusted Macs carry your \(claude(holdsCodex))traffic but never read it."
    }

    public static func shareStateOn(names: [String], holdsCodex: Bool) -> String {
        "On. \(PeerFormat.list(names)) may serve your \(claude(holdsCodex))requests."
    }

    /// What On would do, while no Mac is trusted.
    public static func yesCannotShare(holdsCodex: Bool) -> String {
        "On means a trusted Mac may serve your \(claude(holdsCodex))requests with its own "
            + "accounts, and read them. There is nothing to share with yet."
    }

    /// What On would do, naming the first trusted Mac when there is one.
    public static func yesOff(firstName: String?, holdsCodex: Bool) -> String {
        let who = firstName.map { "\($0) may" } ?? "a trusted Mac may"
        return "On means \(who) serve your \(claude(holdsCodex))requests with its accounts, and "
            + "read them. Which Mac, how much and for how long: Settings."
    }

    // MARK: The Peers tab's rows

    /// What Trust buys, on a found Mac that announced its name.
    public static func untrustedPromise(title: String, holdsCodex: Bool) -> String {
        "Trust shows the same six digits here and on \(title). Press Trust on both and it can "
            + "carry your \(claude(holdsCodex))traffic without reading it."
    }

    /// A carrying Mac's meter sentence.
    public static func gatewaySentence(title: String, capBytesPerHour: Int64, holdsCodex: Bool)
        -> String
    {
        "\(title) is carrying your encrypted \(claude(holdsCodex))bytes and can open none of "
            + "them, out of \(PeerFormat.megabytes(capBytesPerHour)) MB an hour."
    }

    // MARK: The Peers tab's section head and pills

    /// What carrying means, said once under the section head.
    public static func carrySentence(holdsCodex: Bool) -> String {
        "A trusted Mac carries your \(claude(holdsCodex))traffic when this Mac has no path of "
            + "its own, and reads none of it."
    }

    /// The `sharing` pill's help, which is also its VoiceOver value.
    public static func sharingPillHelp(holdsCodex: Bool) -> String {
        "Trusted Macs may serve your \(claude(holdsCodex))requests on their own accounts, and "
            + "read them."
    }

    /// The `trusted` pill's help.
    public static func trustedPillHelp(holdsCodex: Bool) -> String {
        "Pinned on both Macs. It can carry your \(claude(holdsCodex))traffic."
    }

    /// The `can carry` pill's help.
    public static func carryPillHelp(holdsCodex: Bool) -> String {
        "May hold your encrypted \(claude(holdsCodex))bytes and can open none of them. The "
            + "path line under the row says whether it is doing so now."
    }

    /// The pill of a Mac this one lends to.
    public static func youLendPillHelp(holdsCodex: Bool) -> String {
        "May serve your \(claude(holdsCodex))requests on its own accounts, and read them."
    }

    /// The pill of a Mac serving this one right now.
    public static func theyLendPillHelp(holdsCodex: Bool) -> String {
        "Serving your \(claude(holdsCodex))requests on its own accounts right now, and reading "
            + "them."
    }

    /// The `in flight` pill's help, on the line that says which Mac is answering.
    public static func inFlightPillHelp(peer: String, holdsCodex: Bool) -> String {
        let requests = holdsCodex ? "Claude requests" : "Requests"
        return "\(requests) of yours that \(peer) is serving on its own accounts, and reading, "
            + "at this moment."
    }

    // MARK: The trust sheet

    /// What the sheet says once the other Mac's key is pinned.
    public static func trustDone(peer: String, holdsCodex: Bool) -> String {
        "Its key is pinned here as \(peer). It can carry your encrypted \(claude(holdsCodex))bytes "
            + "and open none of them; nothing is shared until you turn sharing on. Do the same on "
            + "that Mac, pointed back here, so both sides hold a pin."
    }

    // MARK: Settings › Peers

    /// The Sharing section's switch.
    public static func settingsShareToggle(holdsCodex: Bool) -> String {
        "Share \(claude(holdsCodex))accounts with trusted Macs"
    }

    /// A trusted Mac's carry grant, in its sheet.
    public static func carryGrant(title: String, holdsCodex: Bool) -> String {
        "\(title) may carry your \(claude(holdsCodex))traffic without reading it"
    }

    /// The carry grant's detail line.
    public static func carryGrantDetail(holdsCodex: Bool) -> String {
        "Used when this Mac has no route of its own. It holds your encrypted "
            + "\(claude(holdsCodex))bytes and cannot open them."
    }

    /// A trusted Mac's serve grant, in its sheet.
    public static func serveGrant(title: String, holdsCodex: Bool) -> String {
        "\(title) may serve your \(claude(holdsCodex))requests on its own accounts, and read them"
    }

    /// The Sharing head's sentence, in the plaintext hue.
    public static func sharingHead(holdsCodex: Bool) -> String {
        "A trusted Mac serves your \(claude(holdsCodex))requests in plaintext."
    }

    /// Advanced's route row.
    public static func viaRow(holdsCodex: Bool) -> String {
        let traffic = holdsCodex ? " for your Claude traffic" : ""
        return "auto picks a trusted Mac\(traffic) the moment this one's own route is dead. none "
            + "never routes through a peer; naming one pins it."
    }

    /// The in-flight count's caption, in the defaults sheet and the lease sheet.
    public static func maxInFlight(holdsCodex: Bool) -> String {
        "max in flight. Not a rate limit: how many of your \(claude(holdsCodex))requests one "
            + "Mac may have open at the same moment."
    }

    /// The defaults sheet's closing note.
    public static func defaultsNote(holdsCodex: Bool) -> String {
        "These are what Share \(claude(holdsCodex))accounts writes for every trusted Mac. A Mac "
            + "serving your \(claude(holdsCodex))request reads it in plaintext; your own sign-in "
            + "never leaves this Mac. 7d_oi is Fable's own allowance, which is why it is offered "
            + "separately and defaults to all of it."
    }

    /// `Claude ` before the noun a promise is about, in a fleet holding a Codex
    /// account; nothing in a Claude-only fleet, which reads today's sentence.
    private static func claude(_ holdsCodex: Bool) -> String {
        holdsCodex ? "Claude " : ""
    }
}
