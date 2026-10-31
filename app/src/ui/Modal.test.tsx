// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { DialogHost, alertDialog, confirmDialog, dismissAllDialogs } from "./Modal";

(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;

let host: HTMLDivElement;
let root: Root;

beforeEach(() => {
  host = document.createElement("div");
  document.body.appendChild(host);
  root = createRoot(host);
  act(() => root.render(<DialogHost />));
});

afterEach(() => {
  act(() => dismissAllDialogs());
  act(() => root.unmount());
  host.remove();
});

const $ = <T extends Element = HTMLElement>(sel: string): T | null =>
  document.querySelector<T>(sel);
const click = (el: Element | null) =>
  act(() => {
    el!.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  });
const pressEsc = () =>
  act(() => {
    window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
  });

describe("In-app Modal（替代 window.confirm/alert）", () => {
  it("confirmDialog 打开：消息、中文「确定」「取消」、稳定 testid", () => {
    act(() => {
      void confirmDialog("退出该群？");
    });
    const modal = $('[data-testid="modal"]');
    expect(modal).toBeTruthy();
    expect(modal!.getAttribute("role")).toBe("dialog");
    expect($('[data-testid="modal-message"]')!.textContent).toBe("退出该群？");
    expect($('[data-testid="modal-ok"]')!.textContent).toBe("确定");
    expect($('[data-testid="modal-cancel"]')!.textContent).toBe("取消");
  });

  it("点「确定」→ resolve(true) 并关闭", async () => {
    let p!: Promise<boolean>;
    act(() => {
      p = confirmDialog("删除好友？");
    });
    click($('[data-testid="modal-ok"]'));
    await expect(p).resolves.toBe(true);
    expect($('[data-testid="modal"]')).toBeNull();
  });

  it("点「取消」/ Esc / 点遮罩 → resolve(false)", async () => {
    let p!: Promise<boolean>;
    act(() => {
      p = confirmDialog("解散群聊？");
    });
    click($('[data-testid="modal-cancel"]'));
    await expect(p).resolves.toBe(false);

    act(() => {
      p = confirmDialog("解散群聊？");
    });
    pressEsc();
    await expect(p).resolves.toBe(false);

    act(() => {
      p = confirmDialog("解散群聊？");
    });
    click($('[data-testid="modal-mask"]'));
    await expect(p).resolves.toBe(false);
  });

  it("alertDialog：只有「确定」无「取消」", async () => {
    let p!: Promise<void>;
    act(() => {
      p = alertDialog("操作失败");
    });
    expect($('[data-testid="modal-message"]')!.textContent).toBe("操作失败");
    expect($('[data-testid="modal-cancel"]')).toBeNull();
    click($('[data-testid="modal-ok"]'));
    await p;
    expect($('[data-testid="modal"]')).toBeNull();
  });

  it("danger 语气 + 自定义按钮文案", () => {
    act(() => {
      void confirmDialog("解散后不可恢复", { danger: true, okText: "解散" });
    });
    const ok = $('[data-testid="modal-ok"]')!;
    expect(ok.textContent).toBe("解散");
    expect(ok.className).toContain("danger-fill");
  });

  it("排队：前一个未关闭时后一个等待", async () => {
    let p1!: Promise<boolean>;
    let p2!: Promise<boolean>;
    act(() => {
      p1 = confirmDialog("第一条");
      p2 = confirmDialog("第二条");
    });
    expect($('[data-testid="modal-message"]')!.textContent).toBe("第一条");
    click($('[data-testid="modal-ok"]'));
    await expect(p1).resolves.toBe(true);
    expect($('[data-testid="modal-message"]')!.textContent).toBe("第二条");
    click($('[data-testid="modal-cancel"]'));
    await expect(p2).resolves.toBe(false);
    expect($('[data-testid="modal"]')).toBeNull();
  });
});
