/**
 * Categorise interfaces by what an operator most likely wants to see first.
 * The libpcap enumeration on a docker/libvirt host returns 50+ entries —
 * roughly all of which are virtual bridges, veth peers and VM taps that
 * nobody wants to scroll past to find `any` or the host's real NIC. This
 * groups them so the editor can show "Recommended" inline and stash the
 * rest behind an expander.
 */

import type { CaptureInterface } from "@/types/api"

export interface InterfaceGroups {
  /** "Any" pseudo-device + loopback + real-looking NICs and bridges. */
  recommended: CaptureInterface[]
  /** veth, vnet, virbr, docker bridges, and similar plumbing. */
  virtual: CaptureInterface[]
}

const VIRTUAL_PREFIXES = [
  "veth", // docker / k8s container veth peer
  "vnet", // libvirt VM tap
  "virbr", // libvirt bridge
  "docker", // docker0 etc.
  "br-", // docker user-defined bridge (br-<hash>)
  "cni", // k8s CNI
  "flannel",
  "weave",
  "cali", // calico
  "tap",
  "tun",
]

const ALWAYS_RECOMMENDED = new Set(["any", "lo"])

export function groupInterfaces(all: CaptureInterface[]): InterfaceGroups {
  const recommended: CaptureInterface[] = []
  const virtual: CaptureInterface[] = []
  for (const i of all) {
    if (ALWAYS_RECOMMENDED.has(i.name)) {
      recommended.push(i)
    } else if (isVirtual(i.name)) {
      virtual.push(i)
    } else {
      recommended.push(i)
    }
  }
  // Within recommended, sort: `any` first, then anything with addresses,
  // then alphabetical. Loopback last in the recommended bucket.
  recommended.sort((a, b) => {
    if (a.name === "any") return -1
    if (b.name === "any") return 1
    if (a.name === "lo") return 1
    if (b.name === "lo") return -1
    const aHas = a.addresses.length > 0 ? 1 : 0
    const bHas = b.addresses.length > 0 ? 1 : 0
    if (aHas !== bHas) return bHas - aHas
    return a.name.localeCompare(b.name)
  })
  virtual.sort((a, b) => a.name.localeCompare(b.name))
  return { recommended, virtual }
}

function isVirtual(name: string): boolean {
  return VIRTUAL_PREFIXES.some((p) => name.startsWith(p))
}
