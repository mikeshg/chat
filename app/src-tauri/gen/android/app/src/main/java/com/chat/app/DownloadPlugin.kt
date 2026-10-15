package com.chat.app

import android.app.Activity
import android.content.ContentResolver
import android.content.ContentValues
import android.net.Uri
import android.os.Build
import android.os.Environment
import android.provider.MediaStore
import android.webkit.MimeTypeMap
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.io.IOException
import java.net.HttpURLConnection
import java.net.SocketTimeoutException
import java.net.URL
import java.util.concurrent.Executors

@InvokeArg
class DownloadArgs {
  lateinit var url: String
  lateinit var fileName: String
}

/** 已映射好的中文错误文案（直接 reject 给前端） */
private class Fail(val text: String) : Exception(text)

/**
 * MM4：把附件流式存进公共 Download 集合（MediaStore，API 29+ 无需存储权限）。
 * URL/文件名已由 Rust 侧校验净化；这里不跟重定向、不带凭据；
 * connect 15s / 读空闲 60s，文案与桌面端一致。
 */
@TauriPlugin
class DownloadPlugin(private val activity: Activity) : Plugin(activity) {
  private val executor = Executors.newCachedThreadPool()

  @Command
  fun download(invoke: Invoke) {
    val args = try {
      invoke.parseArgs(DownloadArgs::class.java)
    } catch (e: Exception) {
      invoke.reject("下载失败：参数错误")
      return
    }
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) {
      invoke.reject("当前 Android 版本不支持直接保存，请升级到 Android 10 及以上")
      return
    }
    executor.execute {
      try {
        invoke.resolve(doDownload(args.url, args.fileName))
      } catch (e: Fail) {
        invoke.reject(e.text)
      } catch (e: Exception) {
        invoke.reject("下载失败：${e.message ?: e.javaClass.simpleName}")
      }
    }
  }

  /** 同步阻塞执行：HttpURLConnection 读流 → MediaStore pending 行 → 落 IS_PENDING=0 */
  private fun doDownload(url: String, fileName: String): JSObject {
    val conn = (URL(url).openConnection() as HttpURLConnection).apply {
      connectTimeout = 15_000
      readTimeout = 60_000
      instanceFollowRedirects = false // 不跟重定向（与 Rust 侧一致）
    }
    try {
      try {
        conn.connect()
      } catch (e: SocketTimeoutException) {
        throw Fail("下载超时")
      } catch (e: IOException) {
        throw Fail("下载失败：${e.message}")
      }
      val code = try {
        conn.responseCode
      } catch (e: IOException) {
        throw Fail("下载失败：${e.message}")
      }
      if (code == HttpURLConnection.HTTP_NOT_FOUND) throw Fail("文件已撤回或不存在")
      if (code !in 200..299) throw Fail("下载失败（HTTP $code）")

      val resolver = activity.contentResolver
      val values = ContentValues().apply {
        put(MediaStore.Downloads.DISPLAY_NAME, fileName)
        put(MediaStore.Downloads.MIME_TYPE, mimeOf(fileName))
        put(MediaStore.Downloads.RELATIVE_PATH, Environment.DIRECTORY_DOWNLOADS)
        put(MediaStore.Downloads.IS_PENDING, 1)
      }
      val uri = resolver.insert(MediaStore.Downloads.EXTERNAL_CONTENT_URI, values)
        ?: throw Fail("保存失败：无法创建下载项")
      try {
        val out = resolver.openOutputStream(uri, "w")
          ?: throw Fail("保存失败：无法写入下载项")
        out.use { o ->
          conn.inputStream.use { input ->
            val buf = ByteArray(64 * 1024)
            while (true) {
              val n = try {
                input.read(buf)
              } catch (e: SocketTimeoutException) {
                throw Fail("下载超时")
              } catch (e: IOException) {
                throw Fail("下载失败：${e.message}")
              }
              if (n < 0) break
              try {
                o.write(buf, 0, n)
              } catch (e: IOException) {
                throw Fail("保存失败：${e.message}")
              }
            }
          }
        }
        resolver.update(
          uri,
          ContentValues().apply { put(MediaStore.Downloads.IS_PENDING, 0) },
          null,
          null,
        )
      } catch (e: Exception) {
        // 任何中途失败：删掉 pending 行，不留半成品
        try {
          resolver.delete(uri, null, null)
        } catch (_: Exception) {}
        throw e
      }
      return JSObject().apply {
        put("path", uri.toString())
        // MediaStore 撞名会自行加后缀，回报最终显示名
        put("name", displayName(resolver, uri) ?: fileName)
      }
    } finally {
      conn.disconnect()
    }
  }

  private fun mimeOf(name: String): String =
    name.substringAfterLast('.', "")
      .takeIf { it.isNotEmpty() }
      ?.let { MimeTypeMap.getSingleton().getMimeTypeFromExtension(it.lowercase()) }
      ?: "application/octet-stream"

  private fun displayName(resolver: ContentResolver, uri: Uri): String? =
    resolver.query(uri, arrayOf(MediaStore.Downloads.DISPLAY_NAME), null, null, null)?.use { c ->
      if (c.moveToFirst() && !c.isNull(0)) c.getString(0) else null
    }
}
