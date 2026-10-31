package com.chat.app

import android.app.Activity
import android.content.ActivityNotFoundException
import android.content.ClipData
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.provider.Settings
import androidx.core.content.FileProvider
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.io.File
import java.io.IOException
import java.net.HttpURLConnection
import java.net.SocketTimeoutException
import java.net.URL
import java.util.concurrent.Executors

@InvokeArg
class ApkDownloadArgs {
  lateinit var url: String
  lateinit var fileName: String
}

@InvokeArg
class ApkInstallArgs {
  lateinit var fileName: String
}

/** 已映射好的中文错误文案（直接 reject 给前端） */
private class ApkFail(val text: String) : Exception(text)

/** 轮询用下载快照：JS 在 download invoke 挂起期间调 progress 拿进度 */
private class ApkDlState {
  @Volatile var state = "idle" // idle | downloading | done | error
  @Volatile var received = 0L
  @Volatile var total = -1L // -1 = 服务器没给 Content-Length
  @Volatile var name = ""
  @Volatile var error = ""
}

/**
 * AS2：应用内 APK 安装器——与附件 DownloadPlugin 分离。
 * 下载进应用私有缓存 cacheDir/apk-update/（不进 MediaStore 公共 Downloads），
 * 安装走 FileProvider content:// URI + 系统包安装器 Intent。
 * 错误前缀约定：PERMISSION_DENIED:… = 未授予安装未知应用权限；
 * INSTALL_FAILED:… = 安装器拉起失败；其余 = 网络/IO。
 */
@TauriPlugin
class ApkInstallerPlugin(private val activity: Activity) : Plugin(activity) {
  private val executor = Executors.newSingleThreadExecutor()
  private val dl = ApkDlState()

  @Command
  fun download(invoke: Invoke) {
    val args = try {
      invoke.parseArgs(ApkDownloadArgs::class.java)
    } catch (e: Exception) {
      invoke.reject("下载失败：参数错误")
      return
    }
    // Rust 侧已净化；这里再挡一次，防插件被别的入口直接调
    if (!safeName(args.fileName) || !args.fileName.lowercase().endsWith(".apk")) {
      invoke.reject("下载失败：参数错误")
      return
    }
    executor.execute {
      try {
        invoke.resolve(doDownload(args.url, args.fileName))
      } catch (e: ApkFail) {
        fail(e.text)
        invoke.reject(e.text)
      } catch (e: Exception) {
        val msg = "下载失败：${e.message ?: e.javaClass.simpleName}"
        fail(msg)
        invoke.reject(msg)
      }
    }
  }

  @Command
  fun progress(invoke: Invoke) {
    invoke.resolve(JSObject().apply {
      put("state", dl.state)
      put("receivedBytes", dl.received)
      put("totalBytes", dl.total)
      put("name", dl.name)
      put("error", dl.error)
    })
  }

  @Command
  fun checkInstallPermission(invoke: Invoke) {
    invoke.resolve(JSObject().apply { put("granted", canInstall()) })
  }

  /** 打开「安装未知应用」授权页；授权后由调用方重试 install */
  @Command
  fun openInstallPermissionSettings(invoke: Invoke) {
    try {
      val intent = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
        Intent(Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES, Uri.parse("package:${activity.packageName}"))
      } else {
        Intent(Settings.ACTION_SECURITY_SETTINGS)
      }
      intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
      activity.startActivity(intent)
      invoke.resolve(JSObject())
    } catch (e: ActivityNotFoundException) {
      invoke.reject("打开设置失败：系统没有对应页面")
    }
  }

  @Command
  fun install(invoke: Invoke) {
    val args = try {
      invoke.parseArgs(ApkInstallArgs::class.java)
    } catch (e: Exception) {
      invoke.reject("安装失败：参数错误")
      return
    }
    if (!canInstall()) {
      invoke.reject("PERMISSION_DENIED:未允许安装未知来源应用")
      return
    }
    val dir = apkDir()
    val file = File(dir, args.fileName)
    // 只装缓存目录内的 .apk：挡穿越，也挡住把任意文件喂给安装器
    if (!safeName(args.fileName) || !args.fileName.lowercase().endsWith(".apk") ||
      !file.isFile || file.canonicalFile.parentFile != dir.canonicalFile
    ) {
      invoke.reject("安装失败：安装包不存在")
      return
    }
    val uri = try {
      FileProvider.getUriForFile(activity, "${activity.packageName}.fileprovider", file)
    } catch (e: Exception) {
      invoke.reject("INSTALL_FAILED:安装包路径不被 FileProvider 支持")
      return
    }
    val intent = Intent(Intent.ACTION_VIEW).apply {
      setDataAndType(uri, "application/vnd.android.package-archive")
      addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_ACTIVITY_NEW_TASK)
      // 部分 ROM 上单靠 FLAG_GRANT 不够，clipData 也带上授权
      clipData = ClipData.newRawUri("", uri)
    }
    try {
      activity.startActivity(intent)
      invoke.resolve(JSObject())
    } catch (e: ActivityNotFoundException) {
      invoke.reject("INSTALL_FAILED:系统没有可用的安装器")
    }
  }

  private fun canInstall(): Boolean =
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
      activity.packageManager.canRequestPackageInstalls()
    } else {
      // API 24/25 没有按应用的安装权限，只有全局「未知来源」开关
      Settings.Secure.getInt(
        activity.contentResolver,
        Settings.Secure.INSTALL_NON_MARKET_APPS,
        0,
      ) == 1
    }

  private fun safeName(name: String): Boolean =
    name.isNotEmpty() && !name.contains('/') && !name.contains('\\') &&
      name != "." && name != ".."

  private fun apkDir(): File = File(activity.cacheDir, "apk-update")

  private fun fail(msg: String) {
    dl.state = "error"
    dl.error = msg
  }

  /** 同步阻塞执行：HttpURLConnection 读流 → <名>.part → 落定改名（不覆盖目录外任何文件） */
  private fun doDownload(url: String, fileName: String): JSObject {
    val dir = apkDir()
    if (!dir.exists() && !dir.mkdirs()) throw ApkFail("保存失败：无法创建缓存目录")
    // 清掉上一版残留，缓存目录只留当前这份
    dir.listFiles()?.forEach { it.delete() }
    dl.state = "downloading"
    dl.received = 0
    dl.total = -1
    dl.name = fileName
    dl.error = ""
    val conn = (URL(url).openConnection() as HttpURLConnection).apply {
      connectTimeout = 15_000
      readTimeout = 60_000
      instanceFollowRedirects = false // 不跟重定向（与 Rust 侧一致；服务器本端已代理到 CDN）
    }
    try {
      try {
        conn.connect()
      } catch (e: SocketTimeoutException) {
        throw ApkFail("下载超时")
      } catch (e: IOException) {
        throw ApkFail("下载失败：${e.message}")
      }
      val code = try {
        conn.responseCode
      } catch (e: IOException) {
        throw ApkFail("下载失败：${e.message}")
      }
      if (code == HttpURLConnection.HTTP_NOT_FOUND) throw ApkFail("安装包不存在或已过期")
      if (code !in 200..299) throw ApkFail("下载失败（HTTP $code）")
      dl.total = conn.getHeaderField("Content-Length")?.toLongOrNull() ?: -1L

      val tmp = File(dir, "$fileName.part")
      val dst = File(dir, fileName)
      try {
        tmp.outputStream().use { o ->
          conn.inputStream.use { input ->
            val buf = ByteArray(64 * 1024)
            while (true) {
              val n = try {
                input.read(buf)
              } catch (e: SocketTimeoutException) {
                throw ApkFail("下载超时")
              } catch (e: IOException) {
                throw ApkFail("下载失败：${e.message}")
              }
              if (n < 0) break
              try {
                o.write(buf, 0, n)
              } catch (e: IOException) {
                throw ApkFail("保存失败：${e.message}")
              }
              dl.received += n
            }
          }
        }
      } catch (e: Exception) {
        tmp.delete() // 任何中途失败：删掉 .part 半成品
        throw e
      }
      if (dst.exists() && !dst.delete()) throw ApkFail("保存失败：无法替换旧安装包")
      if (!tmp.renameTo(dst)) throw ApkFail("保存失败：无法落盘安装包")
      dl.state = "done"
      return JSObject().apply {
        put("path", dst.absolutePath)
        put("name", fileName)
      }
    } finally {
      conn.disconnect()
    }
  }
}
